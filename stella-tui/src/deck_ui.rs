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
use crate::envelope::{
    AgentControl, AgentId, AgentScope, AgentStatus, Inbound, InstalledAgentEntry, Secret, SkillOp,
    SkillScope, SkillSearchHit, SkillsView, WorkspaceInput,
};
use crate::graph::GraphSnapshot;
use crate::input::{ScopeDecision, UserInput};
use crate::scroll::ScrollState;
use crate::splash::SplashState;
use crate::views::mcp::{AuthPrompt, AuthStep, McpMode};

/// How long a turn-stopping Esc stays armed for the double-Esc escalation: a
/// second Esc inside this window (with no other key in between) is "full
/// stop" — cancel, requeue at the front, hold dispatch for the user's next
/// prompt. Outside it, an Esc is just another single stop.
pub const ESC_DOUBLE_WINDOW: std::time::Duration = std::time::Duration::from_secs(3);

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

/// Which pane the AGENTS tab shows — its secondary nav, switched with ←/→
/// (from a blank composer, exactly like the other blank-gated tab keys).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentsPane {
    /// The `htop`-style dashboard of currently ACTIVE agents (the
    /// pre-existing Agents view).
    #[default]
    Executions,
    /// The agents INSTALLED at the user / project level: inspect
    /// name/description/toolbelt, edit (a save is a NEW pinned version),
    /// re-pin an older version, create one from a prompt.
    Installed,
}

/// The INSTALLED AGENTS pane's interaction mode. `Browse` is plain tab
/// state (the composer stays live, like every other tab); every other mode
/// is modal — it owns the keyboard while open, exactly like the queue
/// editor, so its typing never leaks into the composer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InstalledMode {
    #[default]
    Browse,
    /// The definition editor (⏎ on a row opens it): ctrl+s saves — always a
    /// NEW version, immediately pinned; Esc discards the draft.
    Edit,
    /// Create-from-prompt, step 1: type the short description.
    CreateDescribe,
    /// Create-from-prompt, step 2: pick the install scope (project / user).
    CreateScope,
    /// The version picker (`v` on a row): ⏎ re-pins the highlighted version
    /// WITHOUT creating a new one — pin changes never increment the version.
    PickVersion,
}

/// The INSTALLED AGENTS pane's view state. The list itself is driven
/// entirely by [`Inbound::AgentsList`] snapshots the driver sends — the
/// panel owns only selection, the modal sub-states, and their input
/// buffers, exactly like the queue editor owns only `queue_sel`.
#[derive(Debug, Clone)]
pub struct InstalledPanel {
    /// Newest driver snapshot of the installed agents.
    pub entries: Vec<InstalledAgentEntry>,
    /// Selected row in the browse list.
    pub sel: usize,
    /// True once the first [`Inbound::AgentsList`] arrived.
    pub loaded: bool,
    /// An op is in flight driver-side (refresh / save / pin / create) —
    /// cleared when the next list snapshot folds back.
    pub busy: bool,
    /// A transient one-line status/hint (op outcomes, errors).
    pub status: Option<String>,
    pub mode: InstalledMode,
    /// The definition editor's buffer — a full [`Composer`] textarea, the
    /// deck's one editing surface, reused rather than inventing a novel
    /// editor. Paste inserts verbatim (`usize::MAX` chip threshold): an
    /// agent file is exactly the kind of multi-line paste the chip folding
    /// exists to intercept elsewhere.
    pub editor: Composer,
    /// Which agent the editor holds, as `(name, scope)`.
    pub editing: Option<(String, AgentScope)>,
    /// The create-from-prompt description buffer.
    pub create_desc: String,
    /// Scope choice in the create flow: 0 = project, 1 = user.
    pub scope_sel: usize,
    /// Selected row in the version picker.
    pub version_sel: usize,
}

impl Default for InstalledPanel {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            sel: 0,
            loaded: false,
            busy: false,
            status: None,
            mode: InstalledMode::default(),
            editor: Composer::with_paste_threshold(usize::MAX),
            editing: None,
            create_desc: String::new(),
            scope_sel: 0,
            version_sel: 0,
        }
    }
}

impl InstalledPanel {
    /// The browse list's selected entry, if any.
    pub fn selected(&self) -> Option<&InstalledAgentEntry> {
        self.entries.get(self.sel)
    }

    /// The scope the create flow's picker currently points at.
    pub fn create_scope(&self) -> AgentScope {
        if self.scope_sel == 0 {
            AgentScope::Project
        } else {
            AgentScope::User
        }
    }
}

/// Which pane of the SKILLS tab has the keyboard: the installed list or the
/// registry search.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SkillsFocus {
    #[default]
    Installed,
    Search,
}

/// An active SKILLS-tab overlay that captures keys ahead of the list/search
/// panes — the scope picker, the create-description input, the edit buffer, or
/// the version pin picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillPrompt {
    /// Choose install/create scope (project or user) before dispatching.
    Scope {
        action: ScopeAction,
        /// Highlighted choice: `false` = project, `true` = user.
        user: bool,
    },
    /// Type a short description for LLM-assisted creation (then the scope
    /// picker follows).
    CreateDescription { buffer: String },
    /// Edit a skill's body; saving increments its version and pins the new one.
    Edit {
        scope: SkillScope,
        name: String,
        buffer: String,
    },
    /// Pick a version to pin (no edit, no version bump).
    Pin {
        scope: SkillScope,
        name: String,
        latest: u32,
        sel: u32,
    },
}

/// The deferred action a [`SkillPrompt::Scope`] picker resolves into once the
/// user chooses a scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeAction {
    Install { id: String },
    Create { description: String },
}

/// All SKILLS-tab view state. The installed list + `busy`/`status` come from
/// [`Inbound::Skills`] snapshots the driver owns; the rest (selection, the
/// live search query, transient arming, the active overlay) is local.
#[derive(Debug, Clone, Default)]
pub struct SkillsPanel {
    /// The installed-skills read-model, from [`Inbound::Skills`].
    pub view: SkillsView,
    pub focus: SkillsFocus,
    /// Selected row in the installed list.
    pub sel: usize,
    /// The live search-query buffer.
    pub query: String,
    /// Last search results, from [`Inbound::SkillSearch`].
    pub hits: Vec<SkillSearchHit>,
    pub search_sel: usize,
    /// Query changed since the last search → Enter re-searches, not installs.
    pub query_dirty: bool,
    /// An npx search/install is in flight (client-side optimism).
    pub searching: bool,
    /// One-line hint (last op outcome / affordance).
    pub status: Option<String>,
    /// First `ctrl+x` arms; the second uninstalls.
    pub uninstall_armed: bool,
    /// An active overlay capturing keys ahead of the panes.
    pub prompt: Option<SkillPrompt>,
}

/// All ephemeral view state for the deck.
#[derive(Debug, Clone)]
pub struct DeckUi {
    pub tab: DeckTab,
    /// The AGENTS tab's secondary nav: EXECUTIONS | INSTALLED AGENTS.
    pub agents_pane: AgentsPane,
    /// The INSTALLED AGENTS pane's state.
    pub installed: InstalledPanel,
    /// The SKILLS tab's view state (installed list, search, overlays).
    pub skills: SkillsPanel,
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
    /// Whether the Graph tab's file picker is open. While open it is modal
    /// (like the queue editor): printable keys type into its filter, ↑/↓ move
    /// the selection, Enter re-roots the neighborhood on the chosen file, Esc
    /// closes. Opened with `/` (or `Enter`) on the Graph tab.
    pub graph_picker_open: bool,
    /// The picker's filter-as-you-type query — kept separate from the global
    /// composer so opening the picker never disturbs a half-typed prompt.
    pub graph_picker_query: String,
    /// Selected row in the picker, indexing the *filtered* match list
    /// ([`GraphSnapshot::matching_files`]). Reset to 0 on every query edit.
    pub graph_picker_sel: usize,
    /// All MCP-tab state: the configured-servers snapshot, the list cursor, the
    /// search/auth sub-modes and their input buffers (the auth value is
    /// redacted in `Debug`). Out-of-band, driven by [`Inbound::McpServers`] /
    /// [`Inbound::McpSearchResults`].
    pub mcp: crate::views::mcp::McpTabState,
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
    /// Whether the terminal is a *legacy* one (no kitty keyboard protocol).
    /// Enter semantics are now universal — bare `⏎` submits, a modified `⏎`
    /// breaks (see [`crate::composer::classify_enter`]) — so this no longer
    /// gates behavior; it only picks which newline chord the composer footer
    /// advertises: `⌥⏎` on legacy terminals (all that survives), `⌘⏎` where the
    /// chord is reportable. The shell sets it from the terminal capability.
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
    /// When a turn-stopping Esc armed the double-Esc escalation. A second
    /// Esc inside [`ESC_DOUBLE_WINDOW`] with no other key in between sends
    /// [`WorkspaceInput::StopAndHold`]; any other key — or an Esc claimed by
    /// a modal context (popup, editor, gate) — disarms.
    pub esc_armed_at: Option<std::time::Instant>,
    /// Set by a double-Esc: dispatch is held driver-side until the next
    /// submission, which goes out as [`WorkspaceInput::EnqueueFront`] so it
    /// runs before the prompt the hold returned to the queue. Cleared the
    /// moment that submission is sent.
    pub dispatch_held: bool,
    /// The Session tab's incremental transcript fold cache.
    pub session_fold: crate::views::session::SessionFold,
    /// The terminal's color depth, detected once at startup. Render-time-visible
    /// so the progress bar can emit a per-cell ember gradient on truecolor and a
    /// solid flame fill otherwise (an interpolated RGB has no `FALLBACKS` entry,
    /// so it must not reach a 256/16-color terminal).
    pub color_mode: crate::theme::ColorMode,
    /// Motion off-switch (`--no-anim` / `STELLA_NO_ANIM` / `NO_COLOR`): freezes
    /// the progress shimmer, pulse, and caret blink to a static frame.
    pub no_anim: bool,
}

impl Default for DeckUi {
    fn default() -> Self {
        Self {
            tab: DeckTab::Session,
            agents_pane: AgentsPane::default(),
            installed: InstalledPanel::default(),
            skills: SkillsPanel::default(),
            composer: Composer::with_paste_threshold(crate::composer::DECK_PASTE_LINE_THRESHOLD),
            splash: SplashState::new(),
            help_open: false,
            focused: 0,
            session_scroll: ScrollState::default(),
            trace_scroll: ScrollState::default(),
            trace_filter: None,
            graph_cursor: 0,
            graph: None,
            graph_picker_open: false,
            graph_picker_query: String::new(),
            graph_picker_sel: 0,
            mcp: crate::views::mcp::McpTabState::default(),
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
            esc_armed_at: None,
            dispatch_held: false,
            session_fold: crate::views::session::SessionFold::default(),
            color_mode: crate::theme::ColorMode::default(),
            no_anim: false,
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

    /// Route a bracketed paste to whichever editing surface owns the
    /// keyboard: the agent-definition editor while it is open, the global
    /// composer otherwise. Keeps [`crate::deck_shell`] a dumb wire.
    pub fn paste(&mut self, text: &str) {
        if self.installed.mode == InstalledMode::Edit {
            self.installed.editor.paste(text);
        } else {
            self.composer.paste(text);
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
        // A refreshed snapshot re-roots the neighborhood (a file pick, or an
        // `/init` rebuild): the node list is now a different file's, so land
        // the cursor on the new focus (index 0) rather than leaving it on a
        // stale row that render would merely clamp into range.
        ui.graph_cursor = 0;
        return;
    }
    if let Inbound::SlashCommands(commands) = inbound {
        ui.slash_commands = commands.clone();
        ui.slash_selected = 0;
        return;
    }
    // The installed-agents list is out-of-band view state too — the driver
    // owns the definitions on disk and pushes fresh snapshots here; the
    // model fold ignores them.
    if let Inbound::AgentsList { entries, status } = inbound {
        ui.installed.entries = entries.clone();
        ui.installed.loaded = true;
        // A fresh list is the completion signal for a refresh / save / pin /
        // create op — clear the working flag.
        ui.installed.busy = false;
        ui.installed.sel = ui.installed.sel.min(entries.len().saturating_sub(1));
        // A driver-supplied status (op outcome, error) replaces the hint; a
        // plain refresh sends none, leaving any client-side line standing.
        if let Some(status) = status {
            ui.installed.status = Some(status.clone());
        }
        return;
    }
    // Skills snapshots are out-of-band view state (like the graph, the slash
    // vocabulary, and the agents list): applied straight to `DeckUi::skills`.
    if let Inbound::Skills(view) = inbound {
        ui.skills.view = view.clone();
        // A fresh list is the completion signal for a disk/npx op: stop the
        // spinner and disarm any half-armed uninstall.
        ui.skills.searching = false;
        ui.skills.uninstall_armed = false;
        if view.status.is_some() {
            ui.skills.status = view.status.clone();
        }
        let n = ui.skills.view.rows.len();
        ui.skills.sel = if n == 0 { 0 } else { ui.skills.sel.min(n - 1) };
        return;
    }
    if let Inbound::SkillSearch {
        query,
        hits,
        status,
    } = inbound
    {
        if ui.skills.query.trim() == query.trim() {
            ui.skills.query_dirty = false;
        }
        ui.skills.hits = hits.clone();
        ui.skills.search_sel = 0;
        ui.skills.searching = false;
        ui.skills.status = status.clone().or_else(|| {
            Some(if hits.is_empty() {
                format!("no skills found for “{query}”")
            } else {
                format!("{} result(s) — ⏎ installs the selected one", hits.len())
            })
        });
        return;
    }
    if let Inbound::McpServers(servers) = inbound {
        ui.mcp.servers = servers.clone();
        ui.mcp.selected = ui.mcp.selected.min(ui.mcp.servers.len().saturating_sub(1));
        // A refreshed snapshot means the last action landed — clear the
        // transient status once the state it reported is visible.
        ui.mcp.status = None;
        return;
    }
    if let Inbound::McpSearchResults(outcome) = inbound {
        ui.mcp.searching = false;
        ui.mcp.search_selected = 0;
        ui.mcp.search = Some(outcome.clone());
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
///
/// ## Esc precedence
///
/// Esc already carries several meanings; the FIRST matching context wins,
/// top to bottom (each rule is claimed at the corresponding point in this
/// function's flow):
///
/// 1. splash up — any key, Esc included, dismisses it
/// 2. help overlay open — any key closes it
/// 3. queue editor open — Esc closes the editor
/// 4. slash popup active — Esc dismisses the popup (clears the composer)
/// 5. scope-review gate pending, composer empty — Esc aborts the plan
/// 6. Files tab with the diff open — Esc closes the diff
/// 7. Session tab with a message highlighted — Esc clears the highlight
/// 8. armed by a turn-stopping Esc within [`ESC_DOUBLE_WINDOW`], no other
///    key in between — Esc escalates to [`WorkspaceInput::StopAndHold`]
///    (cancel, requeue the interrupted prompt at the front, hold dispatch
///    for the user's next submission)
/// 9. focused agent [`AgentStatus::Running`] — Esc stops the in-flight turn
///    ([`AgentControl::Stop`]; the driver truncates the partial turn and
///    auto-dispatches the next queued prompt)
/// 10. otherwise Esc is ignored
///
/// The composer's content never gates rules 8–9: the cursor always lives in
/// the global composer, so a stop must leave a typed draft untouched. A
/// pending ask-user gate never reaches them either — it folds the agent to
/// [`AgentStatus::WaitingInput`], which fails rule 9's `Running` check.
pub fn handle_deck_key(key: KeyEvent, model: &WorkspaceModel, ui: &mut DeckUi) -> DeckAction {
    if key.kind == KeyEventKind::Release {
        return DeckAction::Ignored;
    }

    let is_ctrl_o =
        key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('o'));
    if !is_ctrl_o {
        ui.ctrl_o_primed = false;
    }

    // The double-Esc pair: EVERY key consumes the armed state up front; only
    // the unclaimed turn-stopping Esc at the tail re-arms, and only an
    // unclaimed second Esc escalates. An Esc claimed by any modal context
    // (popup dismiss, editor close, gate abort, …) therefore breaks the pair
    // exactly like a non-Esc key would.
    let is_esc = matches!(key.code, KeyCode::Esc);
    let esc_armed = ui.esc_armed_at.take();

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

    // The INSTALLED AGENTS sub-modes (editor / create flow / version picker)
    // are modal while open — they own the keyboard (only ctrl+c quit and the
    // splash/help, handled above, precede them), so their typing and their
    // Esc never leak to the composer, the tab views, or the turn-stop rules.
    if ui.installed.mode != InstalledMode::Browse {
        return handle_installed_modal_key(key, ui);
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

    // The Graph tab's file picker is modal exactly like the queue editor: while
    // it is open every key drives the picker (type to filter, ↑/↓ select, Enter
    // re-root, Esc close) — before the composer or any tab handler sees it, so a
    // filter keystroke can never leak into a half-typed prompt.
    if ui.graph_picker_open {
        return handle_graph_picker_key(key, ui);
    }

    let composer_empty = ui.composer.buffer().is_empty();

    // The SKILLS tab is a keyboard-owning manager: it claims the keys for its
    // list, search query, and overlays *ahead* of tab-nav and the global
    // composer, so a search term (or a manage hotkey like space) is never
    // swallowed as prompt text. Keys it declines (`None`) fall through — Tab
    // still leaves the tab, `?` still opens help from the installed pane.
    if ui.tab == DeckTab::Skills
        && let Some(action) = handle_skills_key(key, ui)
    {
        return action;
    }

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
        match classify_enter(&key) {
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

    // Per-tab navigation for non-typing keys…
    if let Some(action) = match ui.tab {
        DeckTab::Agents => handle_agents_key(key, model, ui, composer_empty),
        DeckTab::Traces => handle_traces_key(key, model, ui, composer_empty),
        DeckTab::Graph => handle_graph_key(key, ui, composer_empty),
        DeckTab::Files => handle_files_key(key, model, ui, composer_empty),
        DeckTab::Mcp => handle_mcp_key(key, ui, composer_empty),
        DeckTab::Session => handle_session_key(key, model, ui),
        // The SKILLS tab claims its keys earlier (before the composer), so a
        // key that reaches here fell through on purpose — leave it to the
        // deck-global handlers below.
        DeckTab::Skills => None,
    } {
        return action;
    }

    // …then the turn-interrupt Esc (rules 8–9 of the precedence list): it
    // fires only once every other Esc context has declined the key. The
    // composer never claims Esc, so a typed draft survives both forms.
    if is_esc {
        if esc_armed.is_some_and(|at| at.elapsed() <= ESC_DOUBLE_WINDOW) {
            // Second consecutive Esc inside the window: full stop — cancel,
            // requeue the interrupted prompt at the front, hold dispatch for
            // the user's next submission. Deliberately NOT gated on
            // `Running`: the first Esc's cancel may already have folded
            // (status `Failed`) while the auto-dispatched next prompt has
            // produced no event yet, and that gap is exactly when the second
            // press lands. The driver no-ops if nothing is in flight.
            if let Some(agent) = focused_id(model, ui) {
                ui.dispatch_held = true;
                return DeckAction::Send(WorkspaceInput::StopAndHold { agent });
            }
        } else if let Some(entry) = model.agents.get(ui.focused)
            && entry.status == AgentStatus::Running
        {
            // First Esc: cancel the in-flight turn. The driver truncates the
            // partial turn out of the conversation and auto-dispatches the
            // next queued prompt — "interrupt current, run next".
            ui.esc_armed_at = Some(std::time::Instant::now());
            return DeckAction::Send(WorkspaceInput::Control {
                agent: entry.meta.id.clone(),
                control: AgentControl::Stop,
            });
        }
    }

    handle_composer_key(key, ui)
}

/// Route one submitted prompt. The first submission after a double-Esc hold
/// goes to the FRONT of the queue (and is what releases the hold) so it runs
/// before the returned prompt; everything else appends. This is an explicit
/// [`WorkspaceInput::EnqueueFront`] rather than a driver-side "held"
/// reinterpretation of a plain Enqueue: the deck mirrors every queue edit
/// locally at send time, and a message whose meaning depended on driver
/// state the deck cannot see would let the two queue views drift.
fn submit_prompt(ui: &mut DeckUi, text: String) -> DeckAction {
    if ui.dispatch_held {
        ui.dispatch_held = false;
        DeckAction::Send(WorkspaceInput::EnqueueFront { text })
    } else {
        DeckAction::Send(WorkspaceInput::Enqueue { text })
    }
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
            // `/agents` opens the AGENTS tab directly, on the INSTALLED
            // AGENTS pane (the configured-on-disk view the command has
            // always been about) — and asks the driver, which owns the
            // definitions on disk, for a fresh list.
            "/agents" => {
                ui.set_tab(DeckTab::Agents);
                ui.agents_pane = AgentsPane::Installed;
                ui.installed.busy = true;
                DeckAction::Send(WorkspaceInput::AgentsRefresh)
            }
            // `/skills` opens the SKILLS tab directly and asks the driver —
            // which owns the skills on disk — for a fresh installed list.
            "/skills" => {
                ui.set_tab(DeckTab::Skills);
                ui.skills.status = Some("loading skills…".to_string());
                DeckAction::Send(WorkspaceInput::Skill(SkillOp::List))
            }
            "/mcp" => {
                ui.set_tab(DeckTab::Mcp);
                DeckAction::Handled
            }
            // Everything else — including `/help` — is enqueued for the
            // driver, which owns the session vocabulary and answers into the
            // transcript (a transient overlay would leave no record).
            _ => submit_prompt(ui, text),
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

/// The INSTALLED AGENTS sub-modes' keys, dispatched by the modal gate in
/// [`handle_deck_key`]. Every key is consumed — nothing leaks to the
/// composer or the tab views while a sub-mode is open.
fn handle_installed_modal_key(key: KeyEvent, ui: &mut DeckUi) -> DeckAction {
    match ui.installed.mode {
        InstalledMode::Edit => handle_agent_editor_key(key, ui),
        InstalledMode::CreateDescribe => handle_create_describe_key(key, ui),
        InstalledMode::CreateScope => handle_create_scope_key(key, ui),
        InstalledMode::PickVersion => handle_pick_version_key(key, ui),
        // Unreachable — the gate only fires for non-Browse modes.
        InstalledMode::Browse => DeckAction::Ignored,
    }
}

/// The definition editor: a full textarea over the agent's file content.
/// Every ⏎ is a line break (a file editor's Enter is never "submit");
/// `ctrl+s` saves — always a NEW version, immediately pinned; Esc discards
/// the draft without writing anything.
fn handle_agent_editor_key(key: KeyEvent, ui: &mut DeckUi) -> DeckAction {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Esc => {
            ui.installed.mode = InstalledMode::Browse;
            ui.installed.editing = None;
            ui.installed.status = Some("edit discarded — no version written".into());
            DeckAction::Handled
        }
        KeyCode::Char('s') if ctrl => {
            let Some((name, scope)) = ui.installed.editing.take() else {
                ui.installed.mode = InstalledMode::Browse;
                return DeckAction::Handled;
            };
            let content = ui.installed.editor.buffer().to_string();
            if content.trim().is_empty() {
                // Refuse to save an empty definition — the loader would
                // reject it as EmptyBody and the agent would vanish.
                ui.installed.editing = Some((name, scope));
                ui.installed.status =
                    Some("the definition is empty — Esc to discard instead".into());
                return DeckAction::Handled;
            }
            ui.installed.mode = InstalledMode::Browse;
            ui.installed.busy = true;
            ui.installed.status = Some(format!("saving {name} as a new pinned version…"));
            DeckAction::Send(WorkspaceInput::AgentSave {
                name,
                scope,
                content,
            })
        }
        KeyCode::Enter => {
            ui.installed.editor.insert_newline();
            DeckAction::Handled
        }
        KeyCode::Backspace => {
            ui.installed.editor.backspace();
            DeckAction::Handled
        }
        KeyCode::Char(c)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER | KeyModifiers::META) =>
        {
            ui.installed.editor.insert_char(c);
            DeckAction::Handled
        }
        _ => {
            // Cursor motion (arrows, home/end, the ⌥[ / ⌥] jumps) — then
            // swallow whatever remains: the editor is modal.
            let _ = handle_edit_key(key, &mut ui.installed.editor);
            DeckAction::Handled
        }
    }
}

/// Create-from-prompt, step 1: a one-line description buffer. ⏎ advances to
/// the scope picker; Esc cancels the flow.
fn handle_create_describe_key(key: KeyEvent, ui: &mut DeckUi) -> DeckAction {
    match key.code {
        KeyCode::Esc => {
            ui.installed.mode = InstalledMode::Browse;
            ui.installed.status = None;
            DeckAction::Handled
        }
        KeyCode::Enter => {
            if ui.installed.create_desc.trim().is_empty() {
                ui.installed.status = Some("describe the agent first".into());
            } else {
                ui.installed.mode = InstalledMode::CreateScope;
                ui.installed.status = None;
            }
            DeckAction::Handled
        }
        KeyCode::Backspace => {
            ui.installed.create_desc.pop();
            DeckAction::Handled
        }
        KeyCode::Char(c)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER | KeyModifiers::META) =>
        {
            ui.installed.create_desc.push(c);
            DeckAction::Handled
        }
        _ => DeckAction::Handled,
    }
}

/// Create-from-prompt, step 2: the install-scope picker (project / user,
/// mirroring the skills install flow's scope question). ⏎ dispatches the
/// LLM-assisted creation; Esc steps back to the description.
fn handle_create_scope_key(key: KeyEvent, ui: &mut DeckUi) -> DeckAction {
    match key.code {
        KeyCode::Esc => {
            ui.installed.mode = InstalledMode::CreateDescribe;
            DeckAction::Handled
        }
        KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right => {
            ui.installed.scope_sel = 1 - ui.installed.scope_sel.min(1);
            DeckAction::Handled
        }
        KeyCode::Enter => {
            let description = ui.installed.create_desc.trim().to_string();
            let scope = ui.installed.create_scope();
            ui.installed.mode = InstalledMode::Browse;
            ui.installed.busy = true;
            ui.installed.status = Some(format!(
                "drafting the agent with the session model ({} scope)…",
                scope.label()
            ));
            DeckAction::Send(WorkspaceInput::AgentCreate { description, scope })
        }
        _ => DeckAction::Handled,
    }
}

/// The version picker: ↑/↓ choose, ⏎ re-pins the highlighted version
/// (pin-only — no new version is ever written here), Esc closes.
fn handle_pick_version_key(key: KeyEvent, ui: &mut DeckUi) -> DeckAction {
    let Some(entry) = ui.installed.selected().cloned() else {
        ui.installed.mode = InstalledMode::Browse;
        return DeckAction::Handled;
    };
    let count = entry.versions.len();
    match key.code {
        KeyCode::Esc => {
            ui.installed.mode = InstalledMode::Browse;
            DeckAction::Handled
        }
        KeyCode::Up => {
            ui.installed.version_sel = ui.installed.version_sel.saturating_sub(1);
            DeckAction::Handled
        }
        KeyCode::Down => {
            if count > 0 {
                ui.installed.version_sel = (ui.installed.version_sel + 1).min(count - 1);
            }
            DeckAction::Handled
        }
        KeyCode::Enter => {
            let Some(picked) = entry.versions.get(ui.installed.version_sel) else {
                return DeckAction::Handled;
            };
            ui.installed.mode = InstalledMode::Browse;
            if picked.version == entry.version {
                ui.installed.status = Some(format!(
                    "v{} is already the pinned version of {}",
                    picked.version, entry.name
                ));
                return DeckAction::Handled;
            }
            ui.installed.busy = true;
            ui.installed.status = Some(format!("pinning {} to v{}…", entry.name, picked.version));
            DeckAction::Send(WorkspaceInput::AgentPin {
                name: entry.name,
                scope: entry.scope,
                version: picked.version,
            })
        }
        _ => DeckAction::Handled,
    }
}

/// SKILLS-tab keys. Returns `Some` for keys the tab claims ahead of the
/// composer (nav, manage hotkeys, the search query, overlays), `None` for keys
/// that should fall through to the deck-global handlers — Tab still leaves the
/// tab and `?` still opens help from the installed pane.
fn handle_skills_key(key: KeyEvent, ui: &mut DeckUi) -> Option<DeckAction> {
    // An overlay (scope picker / create / edit / pin) is fully modal.
    if ui.skills.prompt.is_some() {
        return Some(handle_skills_prompt_key(key, ui));
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match ui.skills.focus {
        SkillsFocus::Installed => handle_skills_installed_key(key, ui, ctrl),
        SkillsFocus::Search => handle_skills_search_key(key, ui),
    }
}

/// The installed-skills (manage) pane: navigate, toggle enabled (space),
/// uninstall (ctrl+x twice), edit (e), pin (p), create (n), cross to search (→).
fn handle_skills_installed_key(key: KeyEvent, ui: &mut DeckUi, ctrl: bool) -> Option<DeckAction> {
    let count = ui.skills.view.rows.len();
    // Any key other than a fresh ctrl+x disarms the two-press uninstall.
    let was_armed = ui.skills.uninstall_armed;
    let is_ctrl_x = ctrl && matches!(key.code, KeyCode::Char('x'));
    if !is_ctrl_x {
        ui.skills.uninstall_armed = false;
    }
    match key.code {
        KeyCode::Right => {
            ui.skills.focus = SkillsFocus::Search;
            ui.skills.status = None;
            Some(DeckAction::Handled)
        }
        KeyCode::Up => {
            ui.skills.sel = ui.skills.sel.saturating_sub(1);
            Some(DeckAction::Handled)
        }
        KeyCode::Down => {
            if count > 0 {
                ui.skills.sel = (ui.skills.sel + 1).min(count - 1);
            }
            Some(DeckAction::Handled)
        }
        KeyCode::Char(' ') => {
            let Some(row) = ui.skills.view.rows.get(ui.skills.sel) else {
                return Some(DeckAction::Handled);
            };
            let enabled = !row.enabled;
            let name = row.name.clone();
            let scope = row.scope;
            ui.skills.status = Some(format!(
                "{name} {}",
                if enabled { "enabled" } else { "disabled" }
            ));
            // Optimistic flip; the driver's refreshed snapshot is authoritative.
            if let Some(r) = ui.skills.view.rows.get_mut(ui.skills.sel) {
                r.enabled = enabled;
            }
            Some(DeckAction::Send(WorkspaceInput::Skill(
                SkillOp::SetEnabled {
                    scope,
                    name,
                    enabled,
                },
            )))
        }
        KeyCode::Char('x') if ctrl => {
            let Some(row) = ui.skills.view.rows.get(ui.skills.sel) else {
                return Some(DeckAction::Handled);
            };
            if !row.removable {
                ui.skills.status = Some(format!(
                    "{} can't be deleted here — press space to disable it",
                    row.name
                ));
                return Some(DeckAction::Handled);
            }
            if was_armed {
                let name = row.name.clone();
                let scope = row.scope;
                ui.skills.searching = true;
                ui.skills.status = Some(format!("deleting {name}…"));
                Some(DeckAction::Send(WorkspaceInput::Skill(
                    SkillOp::Uninstall { scope, name },
                )))
            } else {
                ui.skills.uninstall_armed = true;
                ui.skills.status = Some(format!(
                    "press ctrl+x again to DELETE {} ({}) from disk",
                    row.name,
                    row.scope.label()
                ));
                Some(DeckAction::Handled)
            }
        }
        // Edit the selected skill's body (saving makes a new pinned version).
        KeyCode::Char('e') if !ctrl => {
            if let Some(row) = ui.skills.view.rows.get(ui.skills.sel) {
                ui.skills.prompt = Some(SkillPrompt::Edit {
                    scope: row.scope,
                    name: row.name.clone(),
                    buffer: row.body.clone(),
                });
            }
            Some(DeckAction::Handled)
        }
        // Pin a specific version (no edit, no version bump).
        KeyCode::Char('p') if !ctrl => {
            if let Some(row) = ui.skills.view.rows.get(ui.skills.sel) {
                if row.latest <= 1 {
                    ui.skills.status = Some(format!("{} has only one version", row.name));
                } else {
                    ui.skills.prompt = Some(SkillPrompt::Pin {
                        scope: row.scope,
                        name: row.name.clone(),
                        latest: row.latest,
                        sel: row.version,
                    });
                }
            }
            Some(DeckAction::Handled)
        }
        // Create a new skill with LLM assistance from a short description.
        KeyCode::Char('n') if !ctrl => {
            ui.skills.prompt = Some(SkillPrompt::CreateDescription {
                buffer: String::new(),
            });
            Some(DeckAction::Handled)
        }
        _ => None,
    }
}

/// The registry-search pane: type a query, run it (⏎), pick a hit, and install
/// the highlighted one (⏎ again → scope picker). ← returns to the manage pane.
fn handle_skills_search_key(key: KeyEvent, ui: &mut DeckUi) -> Option<DeckAction> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        KeyCode::Left => {
            ui.skills.focus = SkillsFocus::Installed;
            ui.skills.status = None;
            Some(DeckAction::Handled)
        }
        KeyCode::Up => {
            ui.skills.search_sel = ui.skills.search_sel.saturating_sub(1);
            Some(DeckAction::Handled)
        }
        KeyCode::Down => {
            let n = ui.skills.hits.len();
            if n > 0 {
                ui.skills.search_sel = (ui.skills.search_sel + 1).min(n - 1);
            }
            Some(DeckAction::Handled)
        }
        KeyCode::Backspace => {
            ui.skills.query.pop();
            ui.skills.query_dirty = true;
            Some(DeckAction::Handled)
        }
        KeyCode::Enter => {
            if !ui.skills.query_dirty && !ui.skills.hits.is_empty() {
                // Install the highlighted hit — ask the scope first.
                let idx = ui.skills.search_sel.min(ui.skills.hits.len() - 1);
                let id = ui.skills.hits[idx].id.clone();
                ui.skills.prompt = Some(SkillPrompt::Scope {
                    action: ScopeAction::Install { id },
                    user: false,
                });
                Some(DeckAction::Handled)
            } else {
                let query = ui.skills.query.trim().to_string();
                if query.is_empty() {
                    ui.skills.status = Some("type a search term first".into());
                    return Some(DeckAction::Handled);
                }
                ui.skills.searching = true;
                ui.skills.query_dirty = false;
                ui.skills.status = Some(format!("searching “{query}”…"));
                Some(DeckAction::Send(WorkspaceInput::Skill(SkillOp::Search {
                    query,
                })))
            }
        }
        // Printable chars (space included — queries have spaces) type into the
        // query; modified chords are left for the global handlers.
        KeyCode::Char(c) if !ctrl && !alt => {
            ui.skills.query.push(c);
            ui.skills.query_dirty = true;
            Some(DeckAction::Handled)
        }
        _ => None,
    }
}

/// The active SKILLS overlay's keys (fully modal): the scope picker (resolves
/// an install/create into a scoped op), the create-description input, the edit
/// buffer (ctrl+s saves a new version), and the version pin picker.
fn handle_skills_prompt_key(key: KeyEvent, ui: &mut DeckUi) -> DeckAction {
    match ui.skills.prompt.clone() {
        Some(SkillPrompt::Scope { action, user }) => {
            match key.code {
                KeyCode::Esc => {
                    ui.skills.prompt = None;
                    ui.skills.status = Some("cancelled".into());
                    DeckAction::Handled
                }
                KeyCode::Left | KeyCode::Up | KeyCode::Char('p') | KeyCode::Char('P') => {
                    ui.skills.prompt = Some(SkillPrompt::Scope {
                        action,
                        user: false,
                    });
                    DeckAction::Handled
                }
                KeyCode::Right | KeyCode::Down | KeyCode::Char('u') | KeyCode::Char('U') => {
                    ui.skills.prompt = Some(SkillPrompt::Scope { action, user: true });
                    DeckAction::Handled
                }
                KeyCode::Enter => {
                    let scope = if user {
                        SkillScope::User
                    } else {
                        SkillScope::Project
                    };
                    ui.skills.prompt = None;
                    ui.skills.searching = true;
                    match action {
                        ScopeAction::Install { id } => {
                            ui.skills.status =
                                Some(format!("installing {id} for {}…", scope.label()));
                            DeckAction::Send(WorkspaceInput::Skill(SkillOp::Install { scope, id }))
                        }
                        ScopeAction::Create { description } => {
                            ui.skills.status =
                                Some(format!("assembling a skill for {}…", scope.label()));
                            DeckAction::Send(WorkspaceInput::Skill(SkillOp::Create {
                                scope,
                                description,
                            }))
                        }
                    }
                }
                // Modal: swallow everything else.
                _ => DeckAction::Handled,
            }
        }
        // The create-description input: type a short description, ⏎ moves on to
        // the scope picker (which dispatches the LLM-assisted create).
        Some(SkillPrompt::CreateDescription { mut buffer }) => match key.code {
            KeyCode::Esc => {
                ui.skills.prompt = None;
                DeckAction::Handled
            }
            KeyCode::Enter => {
                let description = buffer.trim().to_string();
                if description.is_empty() {
                    ui.skills.status = Some("describe the skill first".into());
                    DeckAction::Handled
                } else {
                    ui.skills.prompt = Some(SkillPrompt::Scope {
                        action: ScopeAction::Create { description },
                        user: false,
                    });
                    DeckAction::Handled
                }
            }
            KeyCode::Backspace => {
                buffer.pop();
                ui.skills.prompt = Some(SkillPrompt::CreateDescription { buffer });
                DeckAction::Handled
            }
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                buffer.push(c);
                ui.skills.prompt = Some(SkillPrompt::CreateDescription { buffer });
                DeckAction::Handled
            }
            _ => DeckAction::Handled,
        },
        // The edit buffer: a minimal textarea. ⏎ inserts a newline; ctrl+s saves
        // (a new pinned version); esc cancels.
        Some(SkillPrompt::Edit {
            scope,
            name,
            mut buffer,
        }) => {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            let alt = key.modifiers.contains(KeyModifiers::ALT);
            match key.code {
                KeyCode::Esc => {
                    ui.skills.prompt = None;
                    ui.skills.status = Some("edit cancelled".into());
                    DeckAction::Handled
                }
                KeyCode::Char('s') if ctrl => {
                    ui.skills.prompt = None;
                    ui.skills.searching = true;
                    ui.skills.status = Some(format!("saving {name}…"));
                    DeckAction::Send(WorkspaceInput::Skill(SkillOp::Edit {
                        scope,
                        name,
                        body: buffer,
                    }))
                }
                KeyCode::Enter => {
                    buffer.push('\n');
                    ui.skills.prompt = Some(SkillPrompt::Edit {
                        scope,
                        name,
                        buffer,
                    });
                    DeckAction::Handled
                }
                KeyCode::Backspace => {
                    buffer.pop();
                    ui.skills.prompt = Some(SkillPrompt::Edit {
                        scope,
                        name,
                        buffer,
                    });
                    DeckAction::Handled
                }
                KeyCode::Char(c) if !ctrl && !alt => {
                    buffer.push(c);
                    ui.skills.prompt = Some(SkillPrompt::Edit {
                        scope,
                        name,
                        buffer,
                    });
                    DeckAction::Handled
                }
                _ => DeckAction::Handled,
            }
        }
        // The pin picker: ↑/↓ choose a version, ⏎ pins it (no version bump).
        Some(SkillPrompt::Pin {
            scope,
            name,
            latest,
            sel,
        }) => match key.code {
            KeyCode::Esc => {
                ui.skills.prompt = None;
                DeckAction::Handled
            }
            KeyCode::Up => {
                ui.skills.prompt = Some(SkillPrompt::Pin {
                    scope,
                    name,
                    latest,
                    sel: sel.saturating_sub(1).max(1),
                });
                DeckAction::Handled
            }
            KeyCode::Down => {
                ui.skills.prompt = Some(SkillPrompt::Pin {
                    scope,
                    name,
                    latest,
                    sel: (sel + 1).min(latest),
                });
                DeckAction::Handled
            }
            KeyCode::Enter => {
                ui.skills.prompt = None;
                ui.skills.searching = true;
                ui.skills.status = Some(format!("pinning {name} to v{sel}…"));
                DeckAction::Send(WorkspaceInput::Skill(SkillOp::Pin {
                    scope,
                    name,
                    version: sel,
                }))
            }
            _ => DeckAction::Handled,
        },
        None => DeckAction::Ignored,
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
                if classify_enter(&key) == EnterAction::Submit
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

/// MCP tab keys. Three sub-modes: Browse (navigate the configured servers and
/// act on the selection), Search (type a registry query, then Enter to search
/// and Enter again to install the highlighted result), and Auth (a two-step
/// masked credential prompt). Search/Auth are modal — they claim every key so
/// typing never leaks into the composer — while Browse's letter actions gate on
/// `composer_empty` so they don't shadow the first character of a prompt.
fn handle_mcp_key(key: KeyEvent, ui: &mut DeckUi, composer_empty: bool) -> Option<DeckAction> {
    match ui.mcp.mode {
        McpMode::Browse => handle_mcp_browse_key(key, ui, composer_empty),
        McpMode::Search => Some(handle_mcp_search_key(key, ui)),
        McpMode::Auth => Some(handle_mcp_auth_key(key, ui)),
    }
}

fn handle_mcp_browse_key(
    key: KeyEvent,
    ui: &mut DeckUi,
    composer_empty: bool,
) -> Option<DeckAction> {
    let count = ui.mcp.servers.len();
    match key.code {
        KeyCode::Up => {
            ui.mcp.selected = ui.mcp.selected.saturating_sub(1);
            Some(DeckAction::Handled)
        }
        KeyCode::Down => {
            if count > 0 {
                ui.mcp.selected = (ui.mcp.selected + 1).min(count - 1);
            }
            Some(DeckAction::Handled)
        }
        // Enter search mode. `/` is safe: it only reaches here on a blank
        // composer (else it opens the slash popup).
        KeyCode::Char('/') if composer_empty => {
            ui.mcp.mode = McpMode::Search;
            ui.mcp.status = None;
            Some(DeckAction::Handled)
        }
        // Enable/disable the selected server (session-scoped, live).
        KeyCode::Char('e') | KeyCode::Char(' ') if composer_empty => {
            ui.mcp.selected_server().map(|s| {
                DeckAction::Send(WorkspaceInput::McpToggle {
                    name: s.name.clone(),
                })
            })
        }
        // Enter the auth prompt for the selected server, prefilled with its
        // first configured credential field (if any).
        KeyCode::Char('a') if composer_empty => {
            let server = ui.mcp.selected_server()?;
            let name = server.name.clone();
            let field = server.auth_fields.first().cloned().unwrap_or_default();
            ui.mcp.auth = AuthPrompt {
                server: name,
                field,
                value: String::new(),
                step: AuthStep::Field,
            };
            ui.mcp.mode = McpMode::Auth;
            Some(DeckAction::Handled)
        }
        // Remove the selected server from mcp.toml.
        KeyCode::Char('x') if composer_empty => ui.mcp.selected_server().map(|s| {
            DeckAction::Send(WorkspaceInput::McpRemove {
                name: s.name.clone(),
            })
        }),
        // Rebuild the snapshot.
        KeyCode::Char('r') if composer_empty => Some(DeckAction::Send(WorkspaceInput::McpRefresh)),
        _ => None,
    }
}

fn handle_mcp_search_key(key: KeyEvent, ui: &mut DeckUi) -> DeckAction {
    match key.code {
        KeyCode::Esc => {
            ui.mcp.mode = McpMode::Browse;
            ui.mcp.searching = false;
            DeckAction::Handled
        }
        KeyCode::Backspace => {
            ui.mcp.query.pop();
            DeckAction::Handled
        }
        KeyCode::Up => {
            ui.mcp.search_selected = ui.mcp.search_selected.saturating_sub(1);
            DeckAction::Handled
        }
        KeyCode::Down => {
            let items = ui.mcp.search.as_ref().map(|o| o.items.len()).unwrap_or(0);
            if items > 0 {
                ui.mcp.search_selected = (ui.mcp.search_selected + 1).min(items - 1);
            }
            DeckAction::Handled
        }
        KeyCode::Enter => {
            // Results already match the query → Enter installs the highlight;
            // otherwise Enter runs the search.
            if ui.mcp.results_match_query() {
                match ui.mcp.selected_search_name().map(str::to_string) {
                    Some(name) => {
                        ui.mcp.status = Some(format!("installing {name}…"));
                        DeckAction::Send(WorkspaceInput::McpInstall { name })
                    }
                    None => DeckAction::Handled,
                }
            } else {
                let query = ui.mcp.query.trim().to_string();
                if query.is_empty() {
                    return DeckAction::Handled;
                }
                ui.mcp.searching = true;
                ui.mcp.search = None;
                DeckAction::Send(WorkspaceInput::McpSearch { query })
            }
        }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            ui.mcp.query.push(c);
            DeckAction::Handled
        }
        // Modal: swallow everything else so nothing leaks to the composer.
        _ => DeckAction::Handled,
    }
}

fn handle_mcp_auth_key(key: KeyEvent, ui: &mut DeckUi) -> DeckAction {
    match key.code {
        KeyCode::Esc => {
            ui.mcp.mode = McpMode::Browse;
            ui.mcp.auth = AuthPrompt::default();
            DeckAction::Handled
        }
        KeyCode::Enter => match ui.mcp.auth.step {
            AuthStep::Field => {
                if ui.mcp.auth.field.trim().is_empty() {
                    return DeckAction::Handled;
                }
                ui.mcp.auth.step = AuthStep::Value;
                DeckAction::Handled
            }
            AuthStep::Value => {
                let server = ui.mcp.auth.server.clone();
                let field = ui.mcp.auth.field.trim().to_string();
                let value = std::mem::take(&mut ui.mcp.auth.value);
                ui.mcp.mode = McpMode::Browse;
                ui.mcp.auth = AuthPrompt::default();
                ui.mcp.status = Some(format!("set credential {field} for {server}"));
                DeckAction::Send(WorkspaceInput::McpAuth {
                    server,
                    field,
                    value: Secret::new(value),
                })
            }
        },
        KeyCode::Backspace => {
            match ui.mcp.auth.step {
                AuthStep::Field => {
                    ui.mcp.auth.field.pop();
                }
                AuthStep::Value => {
                    ui.mcp.auth.value.pop();
                }
            }
            DeckAction::Handled
        }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            match ui.mcp.auth.step {
                AuthStep::Field => ui.mcp.auth.field.push(c),
                AuthStep::Value => ui.mcp.auth.value.push(c),
            }
            DeckAction::Handled
        }
        _ => DeckAction::Handled,
    }
}

fn handle_agents_key(
    key: KeyEvent,
    model: &WorkspaceModel,
    ui: &mut DeckUi,
    composer_empty: bool,
) -> Option<DeckAction> {
    // The secondary nav: ←/→ switch EXECUTIONS ↔ INSTALLED AGENTS. These
    // only arrive here with a blank composer (a composer holding text claims
    // ←/→ for cursor motion first), same gate as every other tab key.
    match key.code {
        KeyCode::Left if ui.agents_pane == AgentsPane::Installed => {
            ui.agents_pane = AgentsPane::Executions;
            return Some(DeckAction::Handled);
        }
        KeyCode::Right if ui.agents_pane == AgentsPane::Executions => {
            ui.agents_pane = AgentsPane::Installed;
            // First visit loads the list; after that the driver keeps it
            // fresh after every op, so no re-fetch on every switch.
            if !ui.installed.loaded && !ui.installed.busy {
                ui.installed.busy = true;
                return Some(DeckAction::Send(WorkspaceInput::AgentsRefresh));
            }
            return Some(DeckAction::Handled);
        }
        _ => {}
    }
    if ui.agents_pane == AgentsPane::Installed {
        return handle_installed_browse_key(key, ui, composer_empty);
    }

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

/// The INSTALLED AGENTS pane's browse keys (non-modal — the composer stays
/// live, so every letter verb is gated on a blank composer exactly like the
/// executions pane's `s`): ↑/↓ select · ⏎ edit · `v` versions · `n` new ·
/// `r` reload.
fn handle_installed_browse_key(
    key: KeyEvent,
    ui: &mut DeckUi,
    composer_empty: bool,
) -> Option<DeckAction> {
    let count = ui.installed.entries.len();
    match key.code {
        KeyCode::Up => {
            ui.installed.sel = ui.installed.sel.saturating_sub(1);
            Some(DeckAction::Handled)
        }
        KeyCode::Down => {
            if count > 0 {
                ui.installed.sel = (ui.installed.sel + 1).min(count - 1);
            }
            Some(DeckAction::Handled)
        }
        // ⏎ opens the editor on the selected agent — the queue editor's
        // "pull it out to edit" idiom, over the pinned version's content.
        KeyCode::Enter if composer_empty && key.modifiers.is_empty() => {
            if let Some(entry) = ui.installed.selected().cloned() {
                ui.installed.editor = Composer::with_paste_threshold(usize::MAX);
                ui.installed.editor.load(entry.content.clone());
                ui.installed.editing = Some((entry.name.clone(), entry.scope));
                ui.installed.mode = InstalledMode::Edit;
                ui.installed.status = None;
            }
            Some(DeckAction::Handled)
        }
        // `v` opens the version picker on the selected agent, preselecting
        // the pinned version.
        KeyCode::Char('v') if composer_empty => {
            if let Some(entry) = ui.installed.selected() {
                ui.installed.version_sel = entry
                    .versions
                    .iter()
                    .position(|v| v.version == entry.version)
                    .unwrap_or(0);
                ui.installed.mode = InstalledMode::PickVersion;
                ui.installed.status = None;
            }
            Some(DeckAction::Handled)
        }
        // `n` starts the create-from-prompt flow.
        KeyCode::Char('n') if composer_empty => {
            ui.installed.mode = InstalledMode::CreateDescribe;
            ui.installed.create_desc.clear();
            ui.installed.scope_sel = 0;
            ui.installed.status = None;
            Some(DeckAction::Handled)
        }
        // `r` reloads the list from disk.
        KeyCode::Char('r') if composer_empty => {
            ui.installed.busy = true;
            ui.installed.status = Some("reloading installed agents…".into());
            Some(DeckAction::Send(WorkspaceInput::AgentsRefresh))
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
        // `/` (filter-as-you-type) or Enter opens the file picker so a user can
        // re-root the neighborhood on any indexed file, not just the busiest
        // one the tab seeds. Gated on an empty composer so both keys stay
        // typeable as the first character of a prompt. Only meaningful once a
        // graph with a file list has loaded.
        KeyCode::Char('/') | KeyCode::Enter if composer_empty && graph_has_files(ui) => {
            open_graph_picker(ui);
            Some(DeckAction::Handled)
        }
        _ => None,
    }
}

/// Whether a graph snapshot with at least one listed file is loaded — the
/// precondition for opening the file picker.
fn graph_has_files(ui: &DeckUi) -> bool {
    ui.graph.as_ref().is_some_and(|g| !g.files.is_empty())
}

/// Open the file picker, defaulting the selection to the file the neighborhood
/// is currently rooted on (`focus`) — the busiest file on first load. That
/// keeps the sensible default while making every other file reachable: the
/// selection starts on "where you already are", not forced there.
fn open_graph_picker(ui: &mut DeckUi) {
    ui.graph_picker_query.clear();
    ui.graph_picker_open = true;
    ui.graph_picker_sel = ui
        .graph
        .as_ref()
        .and_then(|g| g.files.iter().position(|f| *f == g.focus))
        .unwrap_or(0);
}

/// The modal file picker's key map. Printable keys narrow the filter, ↑/↓ walk
/// the filtered matches, Enter re-roots the neighborhood on the selected file
/// (a [`WorkspaceInput::FocusGraphFile`] round-trip — see the envelope docs),
/// and Esc / a cleared-then-Backspace closes it. Selection bounds and the
/// selected path both come from [`GraphSnapshot::matching_files`] so they can
/// never disagree with the rendered list.
fn handle_graph_picker_key(key: KeyEvent, ui: &mut DeckUi) -> DeckAction {
    // Snapshot the current match count/selection off the shared filter helper.
    let match_count = ui
        .graph
        .as_ref()
        .map(|g| g.matching_files(&ui.graph_picker_query).len())
        .unwrap_or(0);

    match key.code {
        KeyCode::Esc => {
            ui.graph_picker_open = false;
            DeckAction::Handled
        }
        KeyCode::Up => {
            ui.graph_picker_sel = ui.graph_picker_sel.saturating_sub(1);
            DeckAction::Handled
        }
        KeyCode::Down => {
            if match_count > 0 {
                ui.graph_picker_sel = (ui.graph_picker_sel + 1).min(match_count - 1);
            }
            DeckAction::Handled
        }
        KeyCode::Enter => {
            let picked = ui.graph.as_ref().and_then(|g| {
                g.matching_files(&ui.graph_picker_query)
                    .get(ui.graph_picker_sel)
                    .map(|f| f.to_string())
            });
            ui.graph_picker_open = false;
            match picked {
                Some(file) => DeckAction::Send(WorkspaceInput::FocusGraphFile { file }),
                None => DeckAction::Handled, // filter matched nothing — just close
            }
        }
        KeyCode::Backspace => {
            ui.graph_picker_query.pop();
            ui.graph_picker_sel = 0; // the match set changed — re-anchor
            DeckAction::Handled
        }
        // Printable characters extend the filter. Modified chords (Ctrl/Cmd)
        // are not filter input — let them fall through as Ignored so global
        // shortcuts still resolve.
        KeyCode::Char(c)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER | KeyModifiers::META) =>
        {
            ui.graph_picker_query.push(c);
            ui.graph_picker_sel = 0; // the match set changed — re-anchor
            DeckAction::Handled
        }
        _ => DeckAction::Ignored,
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
/// entirely; any other prompt ALWAYS enqueues — never blocks on a busy agent,
/// though a held dispatch (see [`submit_prompt`]) jumps it to the front.
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
        Some(text) => submit_prompt(ui, text),
        None => DeckAction::Ignored,
    }
}

/// The always-available composer editing + non-blocking submit. (A non-blank
/// composer's Enter/motion keys were already handled by [`handle_deck_key`]'s
/// textarea interception; this fallback covers typing plus the blank-composer
/// Enter, which submits nothing and inserts nothing.)
fn handle_composer_key(key: KeyEvent, ui: &mut DeckUi) -> DeckAction {
    match classify_enter(&key) {
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
            // Any other prompt ALWAYS enqueues — never blocks on a busy
            // agent. After a double-Esc hold it enqueues at the FRONT.
            Some(text) => submit_prompt(ui, text),
            None => DeckAction::Ignored,
        },
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
    /// The newline chord — `⌘⏎` as the kitty keyboard protocol reports it
    /// (a modified Enter inserts a line break; a bare Enter submits).
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
    fn bare_enter_always_enqueues_a_prompt_without_blocking() {
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
    fn a_modified_enter_inserts_a_line_break_preserved_through_submit() {
        let model = model_with(&["lead"]);
        let mut ui = ready_ui();
        for c in "line one".chars() {
            handle_deck_key(ch(c), &model, &mut ui);
        }
        assert_eq!(
            handle_deck_key(cmd_enter(), &model, &mut ui),
            DeckAction::Handled,
            "⌘⏎ is a line break, not a submit"
        );
        for c in "line two".chars() {
            handle_deck_key(ch(c), &model, &mut ui);
        }
        let action = handle_deck_key(key(KeyCode::Enter), &model, &mut ui);
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
    fn bare_enter_queues_and_a_modified_enter_inserts_a_break() {
        let model = model_with(&["lead"]);
        let mut ui = ready_ui();
        for c in "hi".chars() {
            handle_deck_key(ch(c), &model, &mut ui);
        }
        let alt_enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT);
        assert_eq!(
            handle_deck_key(alt_enter, &model, &mut ui),
            DeckAction::Handled,
            "⌥⏎ inserts a line break"
        );
        assert_eq!(ui.composer.buffer(), "hi\n");
        let action = handle_deck_key(key(KeyCode::Enter), &model, &mut ui);
        assert_eq!(
            action,
            DeckAction::Send(WorkspaceInput::Enqueue {
                text: "hi\n".into()
            }),
            "bare ⏎ queues (never blocks)"
        );
    }

    #[test]
    fn arrow_keys_edit_a_multiline_prompt_instead_of_scrolling() {
        let model = model_with(&["lead"]);
        let mut ui = ready_ui();
        for c in "ab".chars() {
            handle_deck_key(ch(c), &model, &mut ui);
        }
        handle_deck_key(cmd_enter(), &model, &mut ui); // ⌘⏎ inserts a line break
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

    /// A one-agent model whose lead is mid-turn (`Running`).
    fn running_model() -> WorkspaceModel {
        let mut m = model_with(&["lead"]);
        m.apply_inbound(&Inbound::Event {
            agent: "lead".into(),
            event: AgentEvent::Text {
                delta: "working".into(),
            },
        });
        m
    }

    /// The single-Esc outcome: a clean stop for the lead's in-flight turn.
    fn stop_lead() -> DeckAction {
        DeckAction::Send(WorkspaceInput::Control {
            agent: "lead".into(),
            control: AgentControl::Stop,
        })
    }

    #[test]
    fn esc_stops_a_running_turn_and_arms_the_double_press() {
        let model = running_model();
        let mut ui = ready_ui();
        assert_eq!(
            handle_deck_key(key(KeyCode::Esc), &model, &mut ui),
            stop_lead()
        );
        assert!(
            ui.esc_armed_at.is_some(),
            "the stop arms the double-Esc window"
        );
    }

    #[test]
    fn esc_with_no_turn_running_stays_inert() {
        let model = model_with(&["lead"]); // Queued — nothing in flight
        let mut ui = ready_ui();
        assert_eq!(
            handle_deck_key(key(KeyCode::Esc), &model, &mut ui),
            DeckAction::Ignored,
            "an idle Esc must not send a stray stop"
        );
        assert!(ui.esc_armed_at.is_none());
    }

    #[test]
    fn a_typed_draft_survives_both_esc_forms() {
        let model = running_model();
        let mut ui = ready_ui();
        for c in "keep me".chars() {
            handle_deck_key(ch(c), &model, &mut ui);
        }
        // The cursor lives in the global composer, so the stop fires even
        // mid-draft — and must leave the draft untouched.
        assert_eq!(
            handle_deck_key(key(KeyCode::Esc), &model, &mut ui),
            stop_lead()
        );
        assert_eq!(ui.composer.buffer(), "keep me");
        assert_eq!(
            handle_deck_key(key(KeyCode::Esc), &model, &mut ui),
            DeckAction::Send(WorkspaceInput::StopAndHold {
                agent: "lead".into()
            })
        );
        assert_eq!(
            ui.composer.buffer(),
            "keep me",
            "neither cancel form clears what the user typed"
        );
    }

    #[test]
    fn double_esc_inside_the_window_escalates_to_stop_and_hold() {
        let model = running_model();
        let mut ui = ready_ui();
        assert_eq!(
            handle_deck_key(key(KeyCode::Esc), &model, &mut ui),
            stop_lead()
        );
        assert_eq!(
            handle_deck_key(key(KeyCode::Esc), &model, &mut ui),
            DeckAction::Send(WorkspaceInput::StopAndHold {
                agent: "lead".into()
            })
        );
        assert!(
            ui.dispatch_held,
            "the deck now front-inserts its next submission"
        );
        assert!(
            ui.esc_armed_at.is_none(),
            "the pair resets after escalating"
        );
    }

    #[test]
    fn the_second_esc_fires_even_if_the_cancel_already_folded() {
        // Between the two presses the first cancel's error event may fold
        // (status `Failed`) before the auto-dispatched next prompt produces
        // any event — the escalation must not be lost to that gap.
        let mut model = running_model();
        let mut ui = ready_ui();
        assert_eq!(
            handle_deck_key(key(KeyCode::Esc), &model, &mut ui),
            stop_lead()
        );
        model.apply_inbound(&Inbound::Event {
            agent: "lead".into(),
            event: AgentEvent::Error {
                message: "turn stopped by user".into(),
                retryable: false,
            },
        });
        assert_eq!(
            handle_deck_key(key(KeyCode::Esc), &model, &mut ui),
            DeckAction::Send(WorkspaceInput::StopAndHold {
                agent: "lead".into()
            })
        );
    }

    #[test]
    fn an_intervening_key_breaks_the_double_esc_pair() {
        let model = running_model();
        let mut ui = ready_ui();
        handle_deck_key(key(KeyCode::Esc), &model, &mut ui);
        handle_deck_key(ch('x'), &model, &mut ui); // types into the composer
        assert!(ui.esc_armed_at.is_none(), "any other key disarms");
        assert_eq!(
            handle_deck_key(key(KeyCode::Esc), &model, &mut ui),
            stop_lead(),
            "the next Esc is a fresh single stop, not an escalation"
        );
    }

    #[test]
    fn a_stale_arm_outside_the_window_does_not_escalate() {
        let model = running_model();
        let mut ui = ready_ui();
        // Backdate the arm past the window. (If the monotonic clock is too
        // young to backdate, `checked_sub` leaves it unarmed — which expects
        // the same single-stop outcome.)
        ui.esc_armed_at = std::time::Instant::now()
            .checked_sub(ESC_DOUBLE_WINDOW + std::time::Duration::from_secs(1));
        assert_eq!(
            handle_deck_key(key(KeyCode::Esc), &model, &mut ui),
            stop_lead(),
            "past the window, Esc is a single stop again"
        );
    }

    #[test]
    fn esc_dismisses_the_slash_popup_instead_of_stopping_the_turn() {
        let model = running_model();
        let mut ui = ready_ui();
        ui.slash_commands = vec![SlashCommand::new("/help", "help")];
        handle_deck_key(ch('/'), &model, &mut ui);
        assert_eq!(
            handle_deck_key(key(KeyCode::Esc), &model, &mut ui),
            DeckAction::Handled,
            "rule 4: the popup claims Esc — no stop is sent"
        );
        assert!(ui.esc_armed_at.is_none(), "a claimed Esc never arms");
    }

    #[test]
    fn an_esc_claimed_by_the_queue_editor_breaks_the_pair_too() {
        let mut model = model_with_queue(&["one"]);
        model.apply_inbound(&Inbound::Event {
            agent: "lead".into(),
            event: AgentEvent::Text { delta: "hi".into() },
        });
        let mut ui = ready_ui();
        ui.queue_open = true;
        ui.esc_armed_at = Some(std::time::Instant::now());
        assert_eq!(
            handle_deck_key(key(KeyCode::Esc), &model, &mut ui),
            DeckAction::Handled,
            "rule 3: the queue editor claims Esc — no stop is sent"
        );
        assert!(!ui.queue_open, "…it closed the editor");
        assert!(ui.esc_armed_at.is_none(), "…and broke the double-Esc pair");
    }

    #[test]
    fn esc_still_aborts_a_pending_scope_review() {
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
        assert_eq!(
            handle_deck_key(key(KeyCode::Esc), &model, &mut ui),
            DeckAction::Send(WorkspaceInput::ToAgent {
                agent: "lead".into(),
                input: UserInput::ScopeDecision(ScopeDecision::Abort),
            }),
            "rule 5: the gate claims Esc — never a turn stop"
        );
    }

    #[test]
    fn esc_closes_an_open_diff_before_it_stops_the_turn() {
        let model = running_model();
        let mut ui = ready_ui();
        ui.tab = DeckTab::Files;
        ui.files_diff_open = true;
        assert_eq!(
            handle_deck_key(key(KeyCode::Esc), &model, &mut ui),
            DeckAction::Handled,
            "rule 6: the diff overlay claims Esc"
        );
        assert!(!ui.files_diff_open);
    }

    #[test]
    fn esc_clears_a_session_highlight_before_it_stops_the_turn() {
        let mut model = running_model();
        with_tool_exchange(&mut model, "lead");
        let mut ui = ready_ui();
        handle_deck_key(key(KeyCode::Up), &model, &mut ui);
        assert!(ui.session_selected.is_some());
        assert_eq!(
            handle_deck_key(key(KeyCode::Esc), &model, &mut ui),
            DeckAction::Handled,
            "rule 7: the highlight claims Esc first"
        );
        assert_eq!(ui.session_selected, None);
        // With nothing left to claim it, the NEXT Esc stops the turn (and
        // since the claimed Esc broke the pair, it is a single stop).
        assert_eq!(
            handle_deck_key(key(KeyCode::Esc), &model, &mut ui),
            stop_lead()
        );
    }

    #[test]
    fn the_first_submission_after_a_hold_enqueues_at_the_front() {
        let model = model_with(&["lead"]);
        let mut ui = ready_ui();
        ui.dispatch_held = true;
        for c in "urgent fix".chars() {
            handle_deck_key(ch(c), &model, &mut ui);
        }
        assert_eq!(
            handle_deck_key(key(KeyCode::Enter), &model, &mut ui),
            DeckAction::Send(WorkspaceInput::EnqueueFront {
                text: "urgent fix".into()
            }),
            "the held submission jumps the queue"
        );
        assert!(!ui.dispatch_held, "the submission releases the hold");
        for c in "later".chars() {
            handle_deck_key(ch(c), &model, &mut ui);
        }
        assert_eq!(
            handle_deck_key(key(KeyCode::Enter), &model, &mut ui),
            DeckAction::Send(WorkspaceInput::Enqueue {
                text: "later".into()
            }),
            "after the hold clears, submissions append as usual"
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
        // Force a chip regardless of the deck's (high) paste threshold — this
        // test is about the chip interaction, not where the threshold sits.
        ui.composer = crate::composer::Composer::with_paste_threshold(3);
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
    fn slash_mcp_switches_to_the_mcp_tab() {
        let model = model_with(&["lead"]);
        let mut ui = ready_ui();
        ui.slash_commands = vec![SlashCommand::new("/mcp", "mcp")];
        for c in "/mcp".chars() {
            handle_deck_key(ch(c), &model, &mut ui);
        }
        let action = handle_deck_key(key(KeyCode::Enter), &model, &mut ui);
        assert_eq!(action, DeckAction::Handled, "/mcp is consumed locally");
        assert_eq!(ui.tab, DeckTab::Mcp);
    }

    #[test]
    fn mcp_tab_navigates_toggles_and_enters_search() {
        use crate::envelope::McpServerInfo;
        let model = model_with(&["lead"]);
        let mut ui = ready_ui();
        ui.set_tab(DeckTab::Mcp);
        ui.mcp.servers = vec![
            McpServerInfo {
                name: "github".into(),
                kind: "http".into(),
                enabled: true,
                connected: true,
                health: Some("live".into()),
                tool_count: 3,
                auth_fields: vec!["Authorization".into()],
                calls: 5,
            },
            McpServerInfo {
                name: "fs".into(),
                kind: "stdio".into(),
                enabled: true,
                connected: false,
                health: None,
                tool_count: 0,
                auth_fields: vec![],
                calls: 0,
            },
        ];
        // ↓ moves the selection.
        handle_deck_key(key(KeyCode::Down), &model, &mut ui);
        assert_eq!(ui.mcp.selected, 1);
        handle_deck_key(key(KeyCode::Up), &model, &mut ui);
        assert_eq!(ui.mcp.selected, 0);

        // `e` toggles the selected server (session enable/disable).
        let action = handle_deck_key(ch('e'), &model, &mut ui);
        assert_eq!(
            action,
            DeckAction::Send(WorkspaceInput::McpToggle {
                name: "github".into()
            })
        );

        // `/` enters search mode; typing builds the query; Enter searches.
        handle_deck_key(ch('/'), &model, &mut ui);
        assert_eq!(ui.mcp.mode, crate::views::mcp::McpMode::Search);
        for c in "git".chars() {
            handle_deck_key(ch(c), &model, &mut ui);
        }
        assert_eq!(ui.mcp.query, "git");
        let action = handle_deck_key(key(KeyCode::Enter), &model, &mut ui);
        assert_eq!(
            action,
            DeckAction::Send(WorkspaceInput::McpSearch {
                query: "git".into()
            })
        );
        assert!(ui.mcp.searching);
        // Esc leaves search mode.
        handle_deck_key(key(KeyCode::Esc), &model, &mut ui);
        assert_eq!(ui.mcp.mode, crate::views::mcp::McpMode::Browse);
    }

    #[test]
    fn mcp_auth_prompt_captures_a_masked_value_as_a_redacted_secret() {
        let model = model_with(&["lead"]);
        let mut ui = ready_ui();
        ui.set_tab(DeckTab::Mcp);
        ui.mcp.servers = vec![crate::envelope::McpServerInfo {
            name: "github".into(),
            kind: "http".into(),
            enabled: true,
            connected: true,
            health: Some("live".into()),
            tool_count: 1,
            auth_fields: vec![],
            calls: 0,
        }];
        // `a` enters auth mode.
        handle_deck_key(ch('a'), &model, &mut ui);
        assert_eq!(ui.mcp.mode, crate::views::mcp::McpMode::Auth);
        // Type the field name, Enter advances to the value step.
        for c in "TOKEN".chars() {
            handle_deck_key(ch(c), &model, &mut ui);
        }
        handle_deck_key(key(KeyCode::Enter), &model, &mut ui);
        assert_eq!(ui.mcp.auth.step, crate::views::mcp::AuthStep::Value);
        for c in "sk-secret".chars() {
            handle_deck_key(ch(c), &model, &mut ui);
        }
        let action = handle_deck_key(key(KeyCode::Enter), &model, &mut ui);
        match action {
            DeckAction::Send(WorkspaceInput::McpAuth {
                server,
                field,
                value,
            }) => {
                assert_eq!(server, "github");
                assert_eq!(field, "TOKEN");
                assert_eq!(value.reveal(), "sk-secret");
                // The secret never appears under Debug.
                assert!(!format!("{value:?}").contains("sk-secret"));
            }
            other => panic!("expected McpAuth, got {other:?}"),
        }
        assert_eq!(ui.mcp.mode, crate::views::mcp::McpMode::Browse);
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
            files: vec!["src/lib.rs".into()],
        };
        ingest_inbound(
            &Inbound::GraphSnapshot(snapshot.clone()),
            &mut model,
            &mut ui,
        );
        assert_eq!(ui.graph.as_ref(), Some(&snapshot));
    }

    // ── Graph file picker ────────────────────────────────────────────────────

    /// A three-file graph rooted on the busiest (`src/b.rs`), on the Graph tab.
    fn ui_with_graph() -> DeckUi {
        use crate::graph::{GraphNode, GraphSnapshot};
        let mut ui = ready_ui();
        ui.tab = DeckTab::Graph;
        ui.graph = Some(GraphSnapshot {
            focus: "src/b.rs".into(),
            nodes: vec![GraphNode {
                label: "src/b.rs".into(),
                kind: "file".into(),
                location: Some("src/b.rs".into()),
            }],
            edges: vec![],
            files: vec!["src/a.rs".into(), "src/b.rs".into(), "src/c.rs".into()],
        });
        ui
    }

    // ---- AGENTS tab: INSTALLED AGENTS pane -------------------------------

    fn installed_entry(name: &str, version: u32) -> InstalledAgentEntry {
        InstalledAgentEntry {
            name: name.into(),
            description: format!("about {name}"),
            tools: Some(vec!["Read".into()]),
            scope: AgentScope::Project,
            source_path: format!("/ws/.stella/agents/{name}.md"),
            version,
            versions: (1..=version)
                .map(|v| crate::envelope::AgentVersionInfo {
                    version: v,
                    label: String::new(),
                })
                .collect(),
            content: format!("---\nname: {name}\n---\nbody of {name}"),
        }
    }

    /// A ready deck on the AGENTS tab's INSTALLED pane with `entries` loaded.
    fn installed_ui(entries: Vec<InstalledAgentEntry>) -> DeckUi {
        let mut ui = ready_ui();
        ui.tab = DeckTab::Agents;
        ui.agents_pane = AgentsPane::Installed;
        ui.installed.entries = entries;
        ui.installed.loaded = true;
        ui
    }

    #[test]
    fn slash_opens_the_picker_defaulting_to_the_current_focus() {
        let model = model_with(&["lead"]);
        let mut ui = ui_with_graph();
        let action = handle_deck_key(ch('/'), &model, &mut ui);
        assert_eq!(action, DeckAction::Handled);
        assert!(ui.graph_picker_open, "/ opens the picker on the Graph tab");
        // Default selection is the busiest/focused file (index 1 = src/b.rs),
        // the sensible default — not forced there, just pre-selected.
        assert_eq!(ui.graph_picker_sel, 1);
        assert!(
            ui.composer.buffer().is_empty(),
            "/ did not leak into the prompt"
        );
    }

    #[test]
    fn agents_pane_arrows_switch_and_first_visit_asks_for_the_list() {
        let model = model_with(&["lead"]);
        let mut ui = ready_ui();
        ui.tab = DeckTab::Agents;
        assert_eq!(ui.agents_pane, AgentsPane::Executions, "executions first");
        // → switches to INSTALLED AGENTS; the unloaded list triggers one
        // refresh request.
        let action = handle_deck_key(key(KeyCode::Right), &model, &mut ui);
        assert_eq!(ui.agents_pane, AgentsPane::Installed);
        assert_eq!(action, DeckAction::Send(WorkspaceInput::AgentsRefresh));
        // ← switches back; → again does NOT re-fetch (busy flag pending).
        handle_deck_key(key(KeyCode::Left), &model, &mut ui);
        assert_eq!(ui.agents_pane, AgentsPane::Executions);
        assert_eq!(
            handle_deck_key(key(KeyCode::Right), &model, &mut ui),
            DeckAction::Handled,
            "no duplicate refresh while one is in flight"
        );
    }

    #[test]
    fn enter_also_opens_the_picker() {
        let model = model_with(&["lead"]);
        let mut ui = ui_with_graph();
        handle_deck_key(key(KeyCode::Enter), &model, &mut ui);
        assert!(ui.graph_picker_open);
    }

    #[test]
    fn typing_filters_and_re_anchors_the_selection() {
        let model = model_with(&["lead"]);
        let mut ui = ui_with_graph();
        open_graph_picker(&mut ui);
        // Filter to just "a.rs" — one match, selection re-anchors to 0.
        handle_deck_key(ch('a'), &model, &mut ui);
        assert_eq!(ui.graph_picker_query, "a");
        assert_eq!(ui.graph_picker_sel, 0);
        let matches = ui
            .graph
            .as_ref()
            .unwrap()
            .matching_files(&ui.graph_picker_query);
        assert_eq!(matches, vec!["src/a.rs"]);
    }

    #[test]
    fn enter_in_the_picker_re_roots_on_the_selected_file() {
        let model = model_with(&["lead"]);
        let mut ui = ui_with_graph();
        open_graph_picker(&mut ui);
        // A multi-char needle (`c.rs`) narrows to exactly src/c.rs — a bare
        // `c` would also match the shared `src/` prefix of every file.
        for c in "c.rs".chars() {
            handle_deck_key(ch(c), &model, &mut ui);
        }
        let action = handle_deck_key(key(KeyCode::Enter), &model, &mut ui);
        assert_eq!(
            action,
            DeckAction::Send(WorkspaceInput::FocusGraphFile {
                file: "src/c.rs".into()
            }),
            "Enter sends the re-root request for the filtered selection"
        );
        assert!(!ui.graph_picker_open, "the picker closes on selection");
    }

    #[test]
    fn down_arrow_walks_the_filtered_matches_and_clamps() {
        let model = model_with(&["lead"]);
        let mut ui = ui_with_graph();
        open_graph_picker(&mut ui);
        ui.graph_picker_sel = 0;
        handle_deck_key(key(KeyCode::Down), &model, &mut ui);
        handle_deck_key(key(KeyCode::Down), &model, &mut ui);
        handle_deck_key(key(KeyCode::Down), &model, &mut ui); // past the end
        assert_eq!(ui.graph_picker_sel, 2, "clamps to the last of three files");
    }

    #[test]
    fn esc_closes_the_picker_without_re_rooting() {
        let model = model_with(&["lead"]);
        let mut ui = ui_with_graph();
        open_graph_picker(&mut ui);
        let action = handle_deck_key(key(KeyCode::Esc), &model, &mut ui);
        assert_eq!(action, DeckAction::Handled);
        assert!(!ui.graph_picker_open);
    }

    #[test]
    fn the_picker_is_modal_over_the_composer() {
        // A printable key while the picker is open filters — it must NOT type
        // into the global composer (the queue-editor modality contract).
        let model = model_with(&["lead"]);
        let mut ui = ui_with_graph();
        open_graph_picker(&mut ui);
        handle_deck_key(ch('b'), &model, &mut ui);
        assert_eq!(ui.graph_picker_query, "b");
        assert!(
            ui.composer.buffer().is_empty(),
            "filter keys never reach the composer"
        );
    }

    #[test]
    fn a_re_rooted_snapshot_resets_the_node_cursor() {
        let mut model = model_with(&["lead"]);
        let mut ui = ui_with_graph();
        ui.graph_cursor = 5; // stale cursor from the previous neighborhood
        use crate::graph::{GraphNode, GraphSnapshot};
        let rerooted = GraphSnapshot {
            focus: "src/a.rs".into(),
            nodes: vec![GraphNode {
                label: "src/a.rs".into(),
                kind: "file".into(),
                location: Some("src/a.rs".into()),
            }],
            edges: vec![],
            files: vec!["src/a.rs".into(), "src/b.rs".into(), "src/c.rs".into()],
        };
        ingest_inbound(&Inbound::GraphSnapshot(rerooted), &mut model, &mut ui);
        assert_eq!(ui.graph_cursor, 0, "the cursor lands on the new focus");
    }

    #[test]
    fn the_picker_does_not_open_without_a_loaded_graph() {
        let model = model_with(&["lead"]);
        let mut ui = ready_ui();
        ui.tab = DeckTab::Graph; // no snapshot loaded
        handle_deck_key(ch('/'), &model, &mut ui);
        assert!(!ui.graph_picker_open, "nothing to pick from — stays closed");
    }

    #[test]
    fn slash_agents_opens_the_tab_on_the_installed_pane() {
        let model = model_with(&["lead"]);
        let mut ui = ready_ui();
        ui.slash_commands = vec![SlashCommand::new("/agents", "agents")];
        for c in "/agents".chars() {
            handle_deck_key(ch(c), &model, &mut ui);
        }
        let action = handle_deck_key(key(KeyCode::Enter), &model, &mut ui);
        assert_eq!(ui.tab, DeckTab::Agents, "/agents opens the Agents tab");
        assert_eq!(ui.agents_pane, AgentsPane::Installed);
        assert_eq!(action, DeckAction::Send(WorkspaceInput::AgentsRefresh));
        assert!(ui.composer.buffer().is_empty(), "the composer cleared");
    }

    #[test]
    fn agents_list_ingest_updates_the_panel_out_of_band_and_clamps() {
        let mut model = model_with(&["lead"]);
        let mut ui = installed_ui(vec![]);
        ui.installed.sel = 5;
        ui.installed.busy = true;
        ingest_inbound(
            &Inbound::AgentsList {
                entries: vec![installed_entry("reviewer", 1)],
                status: Some("saved".into()),
            },
            &mut model,
            &mut ui,
        );
        assert_eq!(ui.installed.entries.len(), 1);
        assert_eq!(ui.installed.sel, 0, "selection clamped to the new list");
        assert!(!ui.installed.busy, "a fresh list completes the op");
        assert_eq!(ui.installed.status.as_deref(), Some("saved"));
        assert_eq!(
            model.agents.len(),
            1,
            "the model fold ignores the out-of-band list"
        );
    }

    #[test]
    fn installed_enter_opens_the_editor_and_ctrl_s_saves_a_new_version() {
        let model = model_with(&["lead"]);
        let mut ui = installed_ui(vec![installed_entry("reviewer", 2)]);
        handle_deck_key(key(KeyCode::Enter), &model, &mut ui);
        assert_eq!(ui.installed.mode, InstalledMode::Edit);
        assert_eq!(
            ui.installed.editor.buffer(),
            "---\nname: reviewer\n---\nbody of reviewer",
            "the editor holds the pinned version's content"
        );
        // Type at the end (the cursor loads at the end of the buffer), with
        // a plain Enter inserting a newline — never submitting.
        handle_deck_key(key(KeyCode::Enter), &model, &mut ui);
        for c in "x".chars() {
            handle_deck_key(ch(c), &model, &mut ui);
        }
        let action = handle_deck_key(ctrl('s'), &model, &mut ui);
        assert_eq!(
            action,
            DeckAction::Send(WorkspaceInput::AgentSave {
                name: "reviewer".into(),
                scope: AgentScope::Project,
                content: "---\nname: reviewer\n---\nbody of reviewer\nx".into(),
            }),
            "ctrl+s sends the edited content — the driver writes a NEW pinned version"
        );
        assert_eq!(ui.installed.mode, InstalledMode::Browse);
        assert!(ui.installed.busy, "save shows the working state");
    }

    #[test]
    fn editor_esc_discards_without_sending_and_typing_never_leaks() {
        let model = model_with(&["lead"]);
        let mut ui = installed_ui(vec![installed_entry("reviewer", 1)]);
        handle_deck_key(key(KeyCode::Enter), &model, &mut ui);
        for c in "abc".chars() {
            handle_deck_key(ch(c), &model, &mut ui);
        }
        assert!(
            ui.composer.buffer().is_empty(),
            "editor typing never reaches the global composer"
        );
        let action = handle_deck_key(key(KeyCode::Esc), &model, &mut ui);
        assert_eq!(action, DeckAction::Handled, "no save is sent");
        assert_eq!(ui.installed.mode, InstalledMode::Browse);
        assert!(
            ui.installed
                .status
                .as_deref()
                .is_some_and(|s| s.contains("discarded")),
            "{:?}",
            ui.installed.status
        );
    }

    #[test]
    fn create_flow_describes_picks_scope_and_dispatches_the_llm_draft() {
        let model = model_with(&["lead"]);
        let mut ui = installed_ui(vec![]);
        handle_deck_key(ch('n'), &model, &mut ui);
        assert_eq!(ui.installed.mode, InstalledMode::CreateDescribe);
        for c in "reviews diffs".chars() {
            handle_deck_key(ch(c), &model, &mut ui);
        }
        assert_eq!(ui.installed.create_desc, "reviews diffs");
        handle_deck_key(key(KeyCode::Enter), &model, &mut ui);
        assert_eq!(
            ui.installed.mode,
            InstalledMode::CreateScope,
            "⏎ advances to the scope picker (mirrors the skills install flow)"
        );
        // Default scope is project; ↓ flips to user, ↑ flips back.
        assert_eq!(ui.installed.create_scope(), AgentScope::Project);
        handle_deck_key(key(KeyCode::Down), &model, &mut ui);
        assert_eq!(ui.installed.create_scope(), AgentScope::User);
        handle_deck_key(key(KeyCode::Up), &model, &mut ui);
        let action = handle_deck_key(key(KeyCode::Enter), &model, &mut ui);
        assert_eq!(
            action,
            DeckAction::Send(WorkspaceInput::AgentCreate {
                description: "reviews diffs".into(),
                scope: AgentScope::Project,
            })
        );
        assert_eq!(ui.installed.mode, InstalledMode::Browse);
        assert!(ui.installed.busy);
    }

    #[test]
    fn create_flow_requires_a_description_and_esc_steps_back() {
        let model = model_with(&["lead"]);
        let mut ui = installed_ui(vec![]);
        handle_deck_key(ch('n'), &model, &mut ui);
        // Empty description: ⏎ refuses to advance.
        handle_deck_key(key(KeyCode::Enter), &model, &mut ui);
        assert_eq!(ui.installed.mode, InstalledMode::CreateDescribe);
        for c in "x".chars() {
            handle_deck_key(ch(c), &model, &mut ui);
        }
        handle_deck_key(key(KeyCode::Enter), &model, &mut ui);
        assert_eq!(ui.installed.mode, InstalledMode::CreateScope);
        // Esc from the scope picker returns to the description, not Browse.
        handle_deck_key(key(KeyCode::Esc), &model, &mut ui);
        assert_eq!(ui.installed.mode, InstalledMode::CreateDescribe);
        handle_deck_key(key(KeyCode::Esc), &model, &mut ui);
        assert_eq!(ui.installed.mode, InstalledMode::Browse);
    }

    #[test]
    fn version_picker_pins_an_older_version_without_editing() {
        let model = model_with(&["lead"]);
        let mut ui = installed_ui(vec![installed_entry("reviewer", 3)]);
        handle_deck_key(ch('v'), &model, &mut ui);
        assert_eq!(ui.installed.mode, InstalledMode::PickVersion);
        assert_eq!(
            ui.installed.version_sel, 2,
            "the picker opens on the pinned version"
        );
        handle_deck_key(key(KeyCode::Up), &model, &mut ui);
        handle_deck_key(key(KeyCode::Up), &model, &mut ui);
        let action = handle_deck_key(key(KeyCode::Enter), &model, &mut ui);
        assert_eq!(
            action,
            DeckAction::Send(WorkspaceInput::AgentPin {
                name: "reviewer".into(),
                scope: AgentScope::Project,
                version: 1,
            }),
            "⏎ re-pins — an AgentPin, never an AgentSave"
        );
        assert_eq!(ui.installed.mode, InstalledMode::Browse);
    }

    #[test]
    fn version_picker_on_the_already_pinned_version_sends_nothing() {
        let model = model_with(&["lead"]);
        let mut ui = installed_ui(vec![installed_entry("reviewer", 2)]);
        handle_deck_key(ch('v'), &model, &mut ui);
        let action = handle_deck_key(key(KeyCode::Enter), &model, &mut ui);
        assert_eq!(action, DeckAction::Handled, "re-pinning the pin is a no-op");
        assert!(
            ui.installed
                .status
                .as_deref()
                .is_some_and(|s| s.contains("already")),
            "{:?}",
            ui.installed.status
        );
    }

    #[test]
    fn installed_browse_letter_verbs_type_when_the_composer_has_text() {
        let model = model_with(&["lead"]);
        let mut ui = installed_ui(vec![installed_entry("reviewer", 1)]);
        handle_deck_key(ch('h'), &model, &mut ui);
        handle_deck_key(ch('n'), &model, &mut ui);
        assert_eq!(
            ui.installed.mode,
            InstalledMode::Browse,
            "a typed `n` is prompt text, not the create verb"
        );
        assert_eq!(ui.composer.buffer(), "hn");
    }

    // ── SKILLS tab ──────────────────────────────────────────────────────────

    // `SkillOp`, `SkillScope`, `SkillSearchHit`, `SkillsView` arrive via
    // `use super::*`; only `SkillRow` is not imported at module scope.
    use crate::envelope::SkillRow;

    fn skills_ui() -> DeckUi {
        let mut ui = ready_ui();
        ui.tab = DeckTab::Skills;
        ui
    }

    fn a_row(name: &str, scope: SkillScope, enabled: bool) -> SkillRow {
        SkillRow {
            scope,
            name: name.to_string(),
            description: "d".to_string(),
            body: "b".to_string(),
            origin: "workspace".to_string(),
            enabled,
            version: 1,
            latest: 1,
            removable: true,
        }
    }

    #[test]
    fn skills_search_pane_types_a_query_and_dispatches_a_search() {
        let model = WorkspaceModel::new();
        let mut ui = skills_ui();
        ui.skills.focus = SkillsFocus::Search;
        for c in "pdf tools".chars() {
            handle_deck_key(ch(c), &model, &mut ui);
        }
        assert_eq!(ui.skills.query, "pdf tools", "typed into the query");
        assert!(
            ui.composer.is_empty(),
            "the global composer never saw the keys"
        );
        let action = handle_deck_key(key(KeyCode::Enter), &model, &mut ui);
        assert_eq!(
            action,
            DeckAction::Send(WorkspaceInput::Skill(SkillOp::Search {
                query: "pdf tools".into()
            }))
        );
    }

    #[test]
    fn skills_enter_on_a_hit_opens_the_scope_prompt_then_installs_scoped() {
        let model = WorkspaceModel::new();
        let mut ui = skills_ui();
        ui.skills.focus = SkillsFocus::Search;
        ui.skills.hits = vec![SkillSearchHit {
            id: "acme/auth".into(),
            label: "acme/auth  oauth".into(),
        }];
        ui.skills.query = "auth".into();
        ui.skills.query_dirty = false;

        let a = handle_deck_key(key(KeyCode::Enter), &model, &mut ui);
        assert_eq!(a, DeckAction::Handled);
        assert!(matches!(
            ui.skills.prompt,
            Some(SkillPrompt::Scope {
                action: ScopeAction::Install { .. },
                ..
            })
        ));
        handle_deck_key(ch('u'), &model, &mut ui);
        let a = handle_deck_key(key(KeyCode::Enter), &model, &mut ui);
        assert_eq!(
            a,
            DeckAction::Send(WorkspaceInput::Skill(SkillOp::Install {
                scope: SkillScope::User,
                id: "acme/auth".into()
            }))
        );
        assert!(ui.skills.prompt.is_none());
    }

    #[test]
    fn skills_space_toggles_and_two_ctrl_x_uninstalls() {
        let model = WorkspaceModel::new();
        let mut ui = skills_ui();
        ui.skills.view = SkillsView {
            rows: vec![a_row("sql-style", SkillScope::Project, true)],
            status: None,
            busy: false,
        };
        let a = handle_deck_key(ch(' '), &model, &mut ui);
        assert_eq!(
            a,
            DeckAction::Send(WorkspaceInput::Skill(SkillOp::SetEnabled {
                scope: SkillScope::Project,
                name: "sql-style".into(),
                enabled: false
            }))
        );
        assert!(!ui.skills.view.rows[0].enabled, "optimistic flip");

        let a1 = handle_deck_key(ctrl('x'), &model, &mut ui);
        assert_eq!(a1, DeckAction::Handled);
        assert!(ui.skills.uninstall_armed);
        let a2 = handle_deck_key(ctrl('x'), &model, &mut ui);
        assert_eq!(
            a2,
            DeckAction::Send(WorkspaceInput::Skill(SkillOp::Uninstall {
                scope: SkillScope::Project,
                name: "sql-style".into()
            }))
        );
    }

    #[test]
    fn skills_e_opens_edit_overlay_and_ctrl_s_saves() {
        let model = WorkspaceModel::new();
        let mut ui = skills_ui();
        let mut r = a_row("sql-style", SkillScope::Project, true);
        r.body = "old body".into();
        ui.skills.view = SkillsView {
            rows: vec![r],
            status: None,
            busy: false,
        };
        handle_deck_key(ch('e'), &model, &mut ui);
        assert!(matches!(
            ui.skills.prompt,
            Some(SkillPrompt::Edit { ref buffer, .. }) if buffer == "old body"
        ));
        for c in " +more".chars() {
            handle_deck_key(ch(c), &model, &mut ui);
        }
        let a = handle_deck_key(ctrl('s'), &model, &mut ui);
        assert_eq!(
            a,
            DeckAction::Send(WorkspaceInput::Skill(SkillOp::Edit {
                scope: SkillScope::Project,
                name: "sql-style".into(),
                body: "old body +more".into(),
            }))
        );
    }

    #[test]
    fn skills_p_opens_pin_picker_and_enter_pins() {
        let model = WorkspaceModel::new();
        let mut ui = skills_ui();
        let mut r = a_row("s", SkillScope::Project, true);
        r.version = 3;
        r.latest = 3;
        ui.skills.view = SkillsView {
            rows: vec![r],
            status: None,
            busy: false,
        };
        handle_deck_key(ch('p'), &model, &mut ui);
        assert!(matches!(
            ui.skills.prompt,
            Some(SkillPrompt::Pin {
                sel: 3,
                latest: 3,
                ..
            })
        ));
        handle_deck_key(key(KeyCode::Up), &model, &mut ui);
        handle_deck_key(key(KeyCode::Up), &model, &mut ui);
        let a = handle_deck_key(key(KeyCode::Enter), &model, &mut ui);
        assert_eq!(
            a,
            DeckAction::Send(WorkspaceInput::Skill(SkillOp::Pin {
                scope: SkillScope::Project,
                name: "s".into(),
                version: 1,
            }))
        );
    }

    #[test]
    fn skills_n_creates_via_description_then_scope() {
        let model = WorkspaceModel::new();
        let mut ui = skills_ui();
        handle_deck_key(ch('n'), &model, &mut ui);
        assert!(matches!(
            ui.skills.prompt,
            Some(SkillPrompt::CreateDescription { .. })
        ));
        for c in "extract tables from pdfs".chars() {
            handle_deck_key(ch(c), &model, &mut ui);
        }
        let a = handle_deck_key(key(KeyCode::Enter), &model, &mut ui);
        assert_eq!(a, DeckAction::Handled);
        assert!(matches!(
            ui.skills.prompt,
            Some(SkillPrompt::Scope {
                action: ScopeAction::Create { .. },
                ..
            })
        ));
        let a = handle_deck_key(key(KeyCode::Enter), &model, &mut ui);
        assert_eq!(
            a,
            DeckAction::Send(WorkspaceInput::Skill(SkillOp::Create {
                scope: SkillScope::Project,
                description: "extract tables from pdfs".into(),
            }))
        );
    }

    #[test]
    fn skills_snapshot_ingest_updates_view_and_clears_searching() {
        let mut model = WorkspaceModel::new();
        let mut ui = skills_ui();
        ui.skills.searching = true;
        let view = SkillsView {
            rows: vec![a_row("a", SkillScope::Project, true)],
            status: Some("done".into()),
            busy: false,
        };
        ingest_inbound(&Inbound::Skills(view), &mut model, &mut ui);
        assert_eq!(ui.skills.view.rows.len(), 1);
        assert!(!ui.skills.searching, "a fresh list clears the spinner");
        assert_eq!(ui.skills.status.as_deref(), Some("done"));
        assert!(
            model.agents.is_empty(),
            "model fold ignores skills snapshots"
        );
    }

    #[test]
    fn skills_tab_still_leaves_via_tab_key() {
        let model = WorkspaceModel::new();
        let mut ui = skills_ui();
        handle_deck_key(key(KeyCode::Tab), &model, &mut ui);
        // The MCP tab now sits after SKILLS in the cycle, so Tab leaves SKILLS
        // for MCP (still proving SKILLS is not a dead end).
        assert_eq!(ui.tab, DeckTab::Mcp, "Tab cycles Skills → Mcp");
    }
}
