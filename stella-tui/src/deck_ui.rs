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
    AgentControl, AgentId, AgentScope, AgentStatus, EntityField, EntityHit, Inbound,
    InstalledAgentEntry, IssueAction, IssueRow, Secret, SkillOp, SkillScope, SkillSearchHit,
    SkillsView, SplashCue, WorkspaceInput,
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
    /// Help-overlay viewport metrics, recorded each frame so the pure key
    /// handler can clamp/scroll — same contract as `session_total`/`_height`.
    pub help_height: usize,
    pub help_total: usize,
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

/// The ctrl+o markdown preview overlay: a scrollable, read-only render of a
/// skill's `SKILL.md`. Opened from either pane — for an installed skill the
/// body is on hand (`SkillRow::body`, so `body` is `Some` immediately); for a
/// registry hit it is fetched (`body` starts `None` = loading, filled by
/// [`Inbound::SkillPreview`] whose `id` must match `pending`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SkillPreview {
    /// Heading shown in the popup border (the skill id or name).
    pub title: String,
    /// A dim sub-line under the title — the `skills.sh` url or the scope/origin.
    pub subtitle: String,
    /// The awaited hit `id` while `body` is `None`; `None` for a local body.
    pub pending: Option<String>,
    /// The markdown body once available; `None` renders a loading state.
    pub body: Option<String>,
    /// Vertical scroll offset in lines, clamped to content at render time.
    pub scroll: u16,
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
    /// The ctrl+o markdown preview overlay (modal, scroll + esc), or `None`.
    pub preview: Option<SkillPreview>,
}

/// The ISSUES tab's interaction mode. `Browse` is plain tab state (the
/// composer stays live, like every other tab); every other mode is modal —
/// it owns the keyboard while open, exactly like the INSTALLED AGENTS
/// sub-modes, so its typing never leaks into the composer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IssuesMode {
    #[default]
    Browse,
    /// A tracker-side search query line (`/`): ⏎ fires
    /// [`WorkspaceInput::IssuesRefresh`] with the query.
    SearchTracker,
    /// The create form (`n`): Title · Body · Labels · Assignee.
    Create,
    /// A one-line comment input for the selected issue (`c`).
    Comment,
    /// A small status-word input for the selected issue (`s`).
    SetStatus,
}

/// The create form's focusable fields, in Tab order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IssueField {
    #[default]
    Title,
    Body,
    Labels,
    Assignee,
}

impl IssueField {
    pub const ALL: [IssueField; 4] = [
        IssueField::Title,
        IssueField::Body,
        IssueField::Labels,
        IssueField::Assignee,
    ];

    fn index(self) -> usize {
        IssueField::ALL.iter().position(|f| *f == self).unwrap_or(0)
    }

    pub fn next(self) -> IssueField {
        IssueField::ALL[(self.index() + 1) % IssueField::ALL.len()]
    }

    pub fn prev(self) -> IssueField {
        IssueField::ALL[(self.index() + IssueField::ALL.len() - 1) % IssueField::ALL.len()]
    }

    /// The type-ahead vocabulary this field searches, if any.
    pub fn entity_field(self) -> Option<EntityField> {
        match self {
            IssueField::Labels => Some(EntityField::Label),
            IssueField::Assignee => Some(EntityField::Assignee),
            IssueField::Title | IssueField::Body => None,
        }
    }
}

/// The reusable type-ahead sub-state behind the create form's Assignee and
/// Labels fields: opened the instant the first character is typed, fed by
/// per-keystroke [`WorkspaceInput::EntitySearch`] requests, and seq-guarded
/// so out-of-order [`Inbound::EntityHits`] replies are dropped (only the
/// newest emitted `seq` is ever applied).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TypeAhead {
    /// Which vocabulary the popup searches (decided by the field it serves).
    pub field: Option<EntityField>,
    /// The last query emitted (display only).
    pub query: String,
    /// The newest hits accepted (kept while the next keystroke's reply is in
    /// flight, so the popup never flickers empty between keystrokes).
    pub hits: Vec<EntityHit>,
    /// Selected row.
    pub sel: usize,
    /// The seq of the newest emitted request — replies with an older seq are
    /// stale and dropped.
    pub seq: u64,
    /// True while the newest request is unanswered.
    pub loading: bool,
}

impl TypeAhead {
    /// Whether the popup is on screen (it exists only while a searching
    /// field owns the keyboard).
    pub fn open(&self) -> bool {
        self.field.is_some()
    }

    pub fn close(&mut self) {
        *self = TypeAhead::default();
    }
}

/// All ISSUES-tab view state. The rows come from [`Inbound::IssuesList`]
/// snapshots the driver sends; everything else (selection, the modal
/// sub-modes and their input buffers, the type-ahead) is local — the same
/// split as [`InstalledPanel`].
#[derive(Debug, Clone)]
pub struct IssuesPanel {
    /// Newest driver snapshot of the issue list.
    pub rows: Vec<IssueRow>,
    /// Selected row in the browse list.
    pub sel: usize,
    /// True once the first [`Inbound::IssuesList`] arrived.
    pub loaded: bool,
    /// A request is in flight driver-side.
    pub busy: bool,
    /// A transient one-line status/hint (op outcomes, errors, the
    /// no-tracker-connected hint).
    pub notice: Option<String>,
    pub mode: IssuesMode,
    /// The tracker-search query buffer (`SearchTracker` mode).
    pub search_query: String,
    /// The one-line input shared by the Comment / SetStatus prompts.
    pub input: String,
    /// The create form's fields. The body reuses [`Composer`] as a plain
    /// textarea (paste inserts verbatim — `usize::MAX` chip threshold, the
    /// same trick as the agent-definition editor).
    pub form_field: IssueField,
    pub form_title: String,
    pub form_body: Composer,
    pub form_labels: String,
    pub form_assignee: String,
    /// The Assignee/Labels type-ahead popup.
    pub typeahead: TypeAhead,
    /// Monotonic per-panel request counter — every emitted request carries
    /// the next value, so replies can be lane-ordered.
    next_seq: u64,
    /// Newest seq expected to answer with [`Inbound::IssuesList`].
    pub list_wait: u64,
    /// Newest seq expected to answer with [`Inbound::IssueActDone`].
    pub act_wait: u64,
}

impl Default for IssuesPanel {
    fn default() -> Self {
        Self {
            rows: Vec::new(),
            sel: 0,
            loaded: false,
            busy: false,
            notice: None,
            mode: IssuesMode::default(),
            search_query: String::new(),
            input: String::new(),
            form_field: IssueField::default(),
            form_title: String::new(),
            form_body: Composer::with_paste_threshold(usize::MAX),
            form_labels: String::new(),
            form_assignee: String::new(),
            typeahead: TypeAhead::default(),
            next_seq: 0,
            list_wait: 0,
            act_wait: 0,
        }
    }
}

impl IssuesPanel {
    /// The browse list's selected row, if any.
    pub fn selected(&self) -> Option<&IssueRow> {
        self.rows.get(self.sel)
    }

    /// The next request seq (monotonic; starts at 1 so the `0` defaults of
    /// the wait lanes always read as "nothing awaited yet").
    fn bump_seq(&mut self) -> u64 {
        self.next_seq += 1;
        self.next_seq
    }

    /// Reset the create form to blank fields (and close the type-ahead).
    fn clear_form(&mut self) {
        self.form_field = IssueField::Title;
        self.form_title.clear();
        self.form_body = Composer::with_paste_threshold(usize::MAX);
        self.form_labels.clear();
        self.form_assignee.clear();
        self.typeahead.close();
    }

    /// A mutable handle on the active single-line entity field's text
    /// (`None` for Title/Body — they have no type-ahead).
    fn entity_text_mut(&mut self) -> Option<(&mut String, EntityField)> {
        match self.form_field {
            IssueField::Labels => Some((&mut self.form_labels, EntityField::Label)),
            IssueField::Assignee => Some((&mut self.form_assignee, EntityField::Assignee)),
            IssueField::Title | IssueField::Body => None,
        }
    }
}

/// The type-ahead query for a field's current text: the assignee strips one
/// leading `@` (typing `@mac` searches `mac`; a bare `@` searches the empty
/// query, which lists all members); the labels field searches the segment
/// after the last comma, since earlier segments are already-picked labels.
pub(crate) fn entity_query(field: EntityField, text: &str) -> String {
    match field {
        EntityField::Assignee => text.trim().trim_start_matches('@').to_string(),
        EntityField::Label => text.rsplit(',').next().unwrap_or(text).trim().to_string(),
    }
}

/// Write a picked hit's `insert` into a field: the assignee is replaced
/// outright; the labels field keeps its already-picked comma-separated
/// segments and swaps the in-progress last segment for the picked label.
pub(crate) fn apply_entity_insert(text: &mut String, field: EntityField, insert: &str) {
    match field {
        EntityField::Assignee => {
            *text = insert.to_string();
        }
        EntityField::Label => {
            let kept: Vec<&str> = {
                let mut segments: Vec<&str> = text.split(',').map(str::trim).collect();
                // The last segment is the partial query being typed — the
                // picked label replaces it.
                segments.pop();
                segments.into_iter().filter(|s| !s.is_empty()).collect()
            };
            *text = if kept.is_empty() {
                insert.to_string()
            } else {
                format!("{}, {insert}", kept.join(", "))
            };
        }
    }
}

/// After any edit to an entity field: open the popup on the first character,
/// close it when the field empties, and (while text exists) emit the
/// per-keystroke [`WorkspaceInput::EntitySearch`] — no debounce, seq-guarded.
/// Returns the input to push onto [`DeckUi::pending_inputs`], if any.
pub(crate) fn typeahead_after_edit(panel: &mut IssuesPanel) -> Option<WorkspaceInput> {
    let (text, field) = {
        let (text, field) = panel.entity_text_mut()?;
        (text.clone(), field)
    };
    if text.is_empty() {
        // Everything deleted: back to the untouched state (the popup opens
        // again the instant the next first character lands).
        panel.typeahead.close();
        return None;
    }
    let seq = panel.bump_seq();
    let query = entity_query(field, &text);
    if panel.typeahead.field != Some(field) {
        // First character in this field: open fresh (stale hits from the
        // other field must never show under this one).
        panel.typeahead = TypeAhead::default();
        panel.typeahead.field = Some(field);
    }
    panel.typeahead.query = query.clone();
    panel.typeahead.seq = seq;
    panel.typeahead.loading = true;
    Some(WorkspaceInput::EntitySearch { field, query, seq })
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
    /// The ISSUES tab's view state (list, create form, type-ahead).
    pub issues: IssuesPanel,
    /// The one global composer — typing works from any tab.
    pub composer: Composer,
    pub splash: SplashState,
    pub help_open: bool,
    /// Vertical scroll for the help overlay (↑/↓, PageUp/Down, Home/End). Kept
    /// separate from the transcript scroll since the overlay is a different
    /// viewport. Reset to the top whenever the overlay opens.
    pub help_scroll: ScrollState,
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
    /// The no-selection `ctrl+o` overlay: every expandable transcript entry
    /// renders expanded, without touching the per-entry `expanded` sets.
    /// `ctrl+o` (still with no selection) or Esc on the Session tab turns it
    /// off — every way in has a way out.
    pub transcript_expand_all: bool,
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
    /// Whether the SESSIONS overlay is open (empty-prompt `←` on the Session
    /// tab, or `/sessions`). Modal while open: ↑/↓ move, `⏎` open (replay),
    /// `a` archive, `x` delete, `r` refresh, Esc/`←` close.
    pub sessions_open: bool,
    /// The machine-wide session registry snapshot ([`Inbound::Sessions`]),
    /// pre-sorted by the driver; the overlay groups it by phase.
    pub sessions: Vec<crate::envelope::SessionInfo>,
    /// Selected row in the overlay's flattened (grouped) row list.
    pub sessions_sel: usize,
    /// Whether the CONTEXT overlay is open (empty-prompt `→` on the Session
    /// tab, or `/context`): active skills + MCP servers for THIS session,
    /// rendered from the already-live `skills`/`mcp` snapshots.
    pub context_open: bool,
    /// Vertical scroll offset (rows) for the CONTEXT overlay; render clamps.
    pub context_scroll: usize,
    /// Whether the INBOX overlay is open (`/inbox`): the persist-until-read
    /// notifications. ↑/↓ move, Enter marks read (and opens the linked
    /// session when there is one), Space marks read, `R` mark all read.
    pub inbox_open: bool,
    /// The notification snapshot ([`Inbound::Notifications`]), newest first.
    /// The footer badge shows its unread count even while the overlay is shut.
    pub notifications: Vec<crate::envelope::NotificationInfo>,
    /// Selected row in the inbox overlay.
    pub inbox_sel: usize,
    /// Driver requests queued by handlers/ingest beyond the one action a key
    /// can return (e.g. opening CONTEXT refreshes both skills and MCP; a
    /// finished OAuth login refreshes the MCP snapshot). The shell drains
    /// this after every key/inbound and forwards each as a submission.
    pub pending_inputs: Vec<WorkspaceInput>,
    /// The ENGINE panel (SETTINGS tab, `/model-*`): the editor for
    /// `settings.json` → `agent_engine_config`, over a driver-owned snapshot
    /// ([`Inbound::EngineConfig`]). Modal while open.
    pub engine: crate::views::engine::EngineOverlay,
}

impl Default for DeckUi {
    fn default() -> Self {
        Self {
            tab: DeckTab::Session,
            agents_pane: AgentsPane::default(),
            installed: InstalledPanel::default(),
            skills: SkillsPanel::default(),
            issues: IssuesPanel::default(),
            composer: Composer::with_paste_threshold(crate::composer::DECK_PASTE_LINE_THRESHOLD),
            splash: SplashState::new(),
            help_open: false,
            help_scroll: ScrollState::default(),
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
            transcript_expand_all: false,
            esc_armed_at: None,
            dispatch_held: false,
            session_fold: crate::views::session::SessionFold::default(),
            color_mode: crate::theme::ColorMode::default(),
            no_anim: false,
            sessions_open: false,
            sessions: Vec::new(),
            sessions_sel: 0,
            context_open: false,
            context_scroll: 0,
            inbox_open: false,
            notifications: Vec::new(),
            inbox_sel: 0,
            pending_inputs: Vec::new(),
            engine: crate::views::engine::EngineOverlay::default(),
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

    /// Route a bracketed paste to whichever surface owns the keyboard, in the
    /// SAME precedence [`handle_deck_key`] routes keystrokes — so a paste lands
    /// in the focused input, never the global composer behind it.
    ///
    /// This is a security boundary as much as a UX one: the most important case
    /// is the MCP credential **value** step, where a pasted secret would
    /// otherwise land — in plaintext — in the composer and could be sent as a
    /// prompt or shown in the transcript. A modal text input always wins; only
    /// when nothing modal owns the keyboard does the composer receive the paste.
    /// Keeps [`crate::deck_shell`] a dumb wire.
    pub fn paste(&mut self, text: &str) {
        // 1. Installed-agents sub-modes are modal while open.
        if self.installed.mode != InstalledMode::Browse {
            match self.installed.mode {
                InstalledMode::Edit => self.installed.editor.paste(text),
                InstalledMode::CreateDescribe => self.installed.create_desc.push_str(text),
                // Scope / version pickers hold no text — swallow, never leak.
                InstalledMode::CreateScope | InstalledMode::PickVersion | InstalledMode::Browse => {
                }
            }
            return;
        }

        // 1b. The ISSUES tab's sub-modes are modal while open: a paste
        //     belongs to whichever input owns the keyboard. The body is the
        //     one multi-line surface (verbatim paste, like the agent
        //     editor); pasting into an entity field re-fires its type-ahead
        //     search exactly like typing would.
        if self.tab == DeckTab::Issues && self.issues.mode != IssuesMode::Browse {
            match self.issues.mode {
                IssuesMode::SearchTracker => {
                    push_single_line(&mut self.issues.search_query, text);
                }
                IssuesMode::Comment | IssuesMode::SetStatus => {
                    push_single_line(&mut self.issues.input, text);
                }
                IssuesMode::Create => match self.issues.form_field {
                    IssueField::Title => push_single_line(&mut self.issues.form_title, text),
                    IssueField::Body => self.issues.form_body.paste(text),
                    IssueField::Labels | IssueField::Assignee => {
                        if let Some((field_text, _)) = self.issues.entity_text_mut() {
                            push_single_line(field_text, text);
                        }
                        if let Some(input) = typeahead_after_edit(&mut self.issues) {
                            self.pending_inputs.push(input);
                        }
                    }
                },
                IssuesMode::Browse => {}
            }
            return;
        }

        // 2. The Graph tab's modal file-filter input.
        if self.graph_picker_open {
            push_single_line(&mut self.graph_picker_query, text);
            return;
        }

        // 2b. The ENGINE panel is modal while focused (SETTINGS tab): a
        //     paste belongs to its inline edit (a prompt, a model list) or
        //     the picker filter — and with neither active it is swallowed,
        //     never the composer's.
        if self.tab == DeckTab::Settings && self.engine.focused {
            if let Some(edit) = self.engine.edit.as_mut() {
                push_single_line(&mut edit.buffer, text);
            } else if let Some(picker) = self.engine.picker.as_mut() {
                push_single_line(&mut picker.query, text);
            }
            return;
        }

        // 3. The SKILLS tab is keyboard-owning: its overlays and search query
        //    claim keys ahead of the composer, so a paste must too.
        if self.tab == DeckTab::Skills {
            match &mut self.skills.prompt {
                Some(SkillPrompt::CreateDescription { buffer })
                | Some(SkillPrompt::Edit { buffer, .. }) => buffer.push_str(text),
                // Scope / pin pickers hold no text.
                Some(SkillPrompt::Scope { .. } | SkillPrompt::Pin { .. }) => {}
                None if self.skills.focus == SkillsFocus::Search => {
                    push_single_line(&mut self.skills.query, text);
                }
                // Installed-list navigation owns no text input: fall through to
                // the composer, matching its printable-char fallthrough.
                None => self.composer.paste(text),
            }
            return;
        }

        // 4. The MCP tab's modal search and credential inputs.
        if self.tab == DeckTab::Mcp {
            match self.mcp.mode {
                McpMode::Search => push_single_line(&mut self.mcp.query, text),
                McpMode::Auth => match self.mcp.auth.step {
                    AuthStep::Field => push_single_line(&mut self.mcp.auth.field, text),
                    // The secret value: NEVER the composer.
                    AuthStep::Value => push_single_line(&mut self.mcp.auth.value, text),
                },
                // Browse list owns no text input — start a prompt like any tab.
                McpMode::Browse => self.composer.paste(text),
            }
            return;
        }

        // 5. Default: the global composer.
        self.composer.paste(text);
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
    if let Inbound::SkillPreview { id, body, status } = inbound {
        // Only fill if this reply still matches the open, still-loading preview
        // — the user may have closed it or re-targeted another hit meanwhile.
        if let Some(preview) = ui.skills.preview.as_mut()
            && preview.pending.as_deref() == Some(id.as_str())
        {
            preview.body = Some(if body.trim().is_empty() {
                "*(no preview available)*".to_string()
            } else {
                body.clone()
            });
            preview.pending = None;
            preview.scroll = 0;
        }
        if let Some(status) = status {
            ui.skills.status = Some(status.clone());
        }
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
    // `/help` from the driver opens the same overlay the `?` key opens. Reset
    // to the top so re-opening via the command always lands at the start.
    if let Inbound::ShowHelp = inbound {
        ui.help_open = true;
        ui.help_scroll = ScrollState::default();
        ui.help_scroll.follow = false;
        ui.help_scroll.top = 0;
        return;
    }
    // The machine-wide session registry snapshot — out-of-band view state
    // for the SESSIONS overlay (and its refreshes after archive/delete).
    if let Inbound::Sessions(sessions) = inbound {
        ui.sessions = sessions.clone();
        ui.sessions_sel = ui.sessions_sel.min(sessions.len().saturating_sub(1));
        return;
    }
    // The persist-until-read notification snapshot: feeds the footer badge
    // continuously and the inbox overlay when open.
    if let Inbound::Notifications(notifications) = inbound {
        ui.notifications = notifications.clone();
        ui.inbox_sel = ui.inbox_sel.min(notifications.len().saturating_sub(1));
        return;
    }
    // OAuth login progress lands on the MCP tab's status line; a successful
    // finish means the token store changed, so the snapshot (⚿ oauth badge)
    // is stale — queue a refresh for the shell to forward.
    if let Inbound::McpOauthStatus {
        server,
        message,
        outcome,
    } = inbound
    {
        ui.mcp.status = Some(match outcome {
            None => format!("{server}: {message}"),
            Some(true) => format!("{server}: ✓ {message}"),
            Some(false) => format!("{server}: ✗ {message}"),
        });
        if *outcome == Some(true) {
            ui.pending_inputs.push(WorkspaceInput::McpRefresh);
        }
        return;
    }
    // The agent-engine configuration snapshot — out-of-band view state for
    // the ENGINE panel. Applied by the overlay's own ingest,
    // which guards unsaved local edits; the model fold never sees it.
    if let Inbound::EngineConfig { state, status } = inbound {
        crate::views::engine::ingest_config(ui, state, status);
        return;
    }
    // The ISSUES tab's out-of-band replies, each lane seq-guarded: only the
    // newest emitted request's answer is applied; anything older is stale
    // and dropped (the per-keystroke type-ahead stream depends on this).
    if let Inbound::IssuesList { seq, outcome } = inbound {
        if *seq < ui.issues.list_wait {
            return;
        }
        ui.issues.busy = false;
        ui.issues.loaded = true;
        match outcome {
            Ok(rows) => {
                ui.issues.rows = rows.clone();
                ui.issues.sel = ui.issues.sel.min(rows.len().saturating_sub(1));
                ui.issues.notice = Some(format!("{} issue(s)", rows.len()));
            }
            Err(e) => ui.issues.notice = Some(e.clone()),
        }
        return;
    }
    if let Inbound::IssueActDone { seq, key, outcome } = inbound {
        if *seq < ui.issues.act_wait {
            return;
        }
        ui.issues.busy = false;
        ui.issues.notice = Some(match outcome {
            Ok(message) => message.clone(),
            Err(e) if key.is_empty() => e.clone(),
            Err(e) => format!("{key}: {e}"),
        });
        return;
    }
    if let Inbound::EntityHits {
        field,
        seq,
        query: _,
        hits,
    } = inbound
    {
        let ta = &mut ui.issues.typeahead;
        // Popup closed, re-targeted to the other field, or an out-of-order
        // reply from an older keystroke — all stale, all dropped.
        if ta.field != Some(*field) || *seq < ta.seq {
            return;
        }
        ta.hits = hits.clone();
        ta.sel = ta.sel.min(hits.len().saturating_sub(1));
        ta.loading = false;
        return;
    }
    // Launch-cinematic cues: the driver replays the splash held open over a
    // running init (`/init`, session startup) and releases it when init
    // finishes. Out-of-band view state like `ShowHelp`. `--no-anim`
    // sessions ignore the replay — their contract is a static frame — and a
    // release on a splash that never held is a harmless no-op.
    if let Inbound::Splash(cue) = inbound {
        match cue {
            SplashCue::Replay => {
                if !ui.no_anim {
                    ui.splash = SplashState::new_held();
                }
            }
            SplashCue::Release => ui.splash.release(),
        }
        return;
    }
    // A deregister removes a dashboard row, shifting every index after it:
    // fold it, then repair the focus so the focused AGENT stays focused when
    // an earlier row vanishes. When the focused row itself is removed, its
    // successor inherits focus (clamped into range below) and the transcript
    // selection drops — it indexed the removed agent's transcript.
    if let Inbound::Deregister { agent } = inbound {
        let removed = model.index_of(agent);
        model.apply_inbound(inbound);
        match removed {
            Some(idx) if idx < ui.focused => ui.focused -= 1,
            Some(idx) if idx == ui.focused && ui.session_selected.take().is_some() => {
                ui.session_pending_scroll = None;
                ui.session_scroll.follow = true;
            }
            _ => {}
        }
        clamp(model, ui);
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
/// 2. help overlay open — Esc/`q`/`?` close it; other keys scroll it
/// 3. queue editor open — Esc closes the editor
/// 4. slash popup active — Esc dismisses the popup (clears the composer)
/// 5. scope-review gate pending, composer empty — Esc aborts the plan
/// 6. Files tab with the diff open — Esc closes the diff
/// 7. Session tab with a message highlighted — Esc clears the highlight
/// 8. Session tab with the ctrl+o expand-ALL overlay on — Esc collapses it
///    (each Esc peels one layer: highlight first, then the overlay, so
///    every way into an expanded view has a graceful way back out)
/// 9. armed by a turn-stopping Esc within [`ESC_DOUBLE_WINDOW`], no other
///    key in between — Esc escalates to [`WorkspaceInput::StopAndHold`]
///    (cancel, requeue the interrupted prompt at the front, hold dispatch
///    for the user's next submission)
/// 10. focused agent [`AgentStatus::Running`] — Esc stops the in-flight turn
///     ([`AgentControl::Stop`]; the driver truncates the partial turn and
///     auto-dispatches the next queued prompt)
/// 11. otherwise Esc is ignored
///
/// The composer's content never gates rules 8–9: the cursor always lives in
/// the global composer, so a stop must leave a typed draft untouched. A
/// pending ask-user gate never reaches them either — it folds the agent to
/// [`AgentStatus::WaitingInput`], which fails rule 9's `Running` check.
/// Append a paste into a single-line input, dropping newlines so a multi-line
/// clipboard blob cannot smuggle extra "lines" into a one-line field (a search
/// query, a credential field, a secret value). Multi-line surfaces (the agent
/// editor, a skill body) take the raw text instead.
fn push_single_line(buf: &mut String, text: &str) {
    buf.extend(text.chars().filter(|&c| c != '\n' && c != '\r'));
}

pub fn handle_deck_key(key: KeyEvent, model: &WorkspaceModel, ui: &mut DeckUi) -> DeckAction {
    if key.kind == KeyEventKind::Release {
        return DeckAction::Ignored;
    }

    let is_ctrl_o =
        key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('o'));

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

    // Help overlay is modal: scrolling keys drive it, `q`/Esc close it. Only
    // Ctrl-C quit and the splash (handled above) precede it. Unlike a plain
    // "any key closes" dismiss, this keeps the overlay readable — the content
    // is long enough to scroll on most terminals.
    if ui.help_open {
        return handle_help_key(key, ui);
    }

    // The INSTALLED AGENTS sub-modes (editor / create flow / version picker)
    // are modal while open — they own the keyboard (only ctrl+c quit and the
    // splash/help, handled above, precede them), so their typing and their
    // Esc never leak to the composer, the tab views, or the turn-stop rules.
    if ui.installed.mode != InstalledMode::Browse {
        return handle_installed_modal_key(key, ui);
    }

    // The ISSUES tab's sub-modes (tracker search / create form / comment /
    // set-status) are modal in exactly the same way while the tab is active:
    // the create form owns Tab (field cycling), Enter, and the type-ahead
    // popup's keys, so it must claim them ahead of the deck-global tab
    // navigation and the composer.
    if ui.tab == DeckTab::Issues && ui.issues.mode != IssuesMode::Browse {
        return handle_issues_modal_key(key, ui);
    }

    // Ctrl-R toggles the collapsed-thinking view from anywhere.
    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('r')) {
        ui.thinking_expanded = !ui.thinking_expanded;
        return DeckAction::Handled;
    }

    // Ctrl-O: the expand/collapse verb. On a ↑/↓-highlighted message it
    // toggles that message; with nothing highlighted (e.g. mid-prompt) it
    // toggles the expand-ALL overlay — every expandable message opens at
    // once, and a second ctrl+o (or Esc on the Session tab) closes them all
    // again. The SKILLS tab reuses ctrl+o for its own markdown preview
    // overlay, so it must NOT be claimed here on that tab — the
    // keyboard-owning skills handler below owns it.
    if is_ctrl_o && ui.tab != DeckTab::Skills {
        if let Some(sel) = ui.session_selected {
            // Only a genuinely expandable entry toggles — a no-op press must
            // not bump `expanded_rev` and invalidate the settled fold cache.
            if let Some(agent) = model.agents.get(ui.focused)
                && agent.model.transcript.get(sel).is_some_and(is_expandable)
            {
                let id = agent.meta.id.clone();
                if ui.transcript_expand_all {
                    // Collapsing ONE row out of the everything-open overlay:
                    // materialize the overlay into the per-entry set first,
                    // so the toggle below closes just the highlighted row and
                    // the rest stay open.
                    let all: std::collections::HashSet<usize> = agent
                        .model
                        .transcript
                        .iter()
                        .enumerate()
                        .filter(|(_, e)| is_expandable(e))
                        .map(|(i, _)| i)
                        .collect();
                    ui.expanded.insert(id.clone(), all);
                    ui.transcript_expand_all = false;
                }
                toggle_expanded(ui, &id, sel);
            }
        } else {
            ui.transcript_expand_all = !ui.transcript_expand_all;
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

    // The ENGINE panel (the SETTINGS tab's config editor) is modal exactly
    // like the queue editor while focused: it owns the keyboard — its inline
    // edit buffer, its model picker, and its letter verbs (`s`/`S`/`x`/`r`)
    // must never leak into the composer. Scoped to its own tab so a stale
    // focus flag can never trap the keyboard elsewhere.
    if ui.tab == DeckTab::Settings && ui.engine.focused {
        return crate::views::engine::handle_engine_key(key, ui);
    }

    // The SESSIONS / INBOX / CONTEXT overlays are modal exactly like the
    // queue editor while open: they own the keyboard until dismissed.
    if ui.sessions_open {
        return handle_sessions_key(key, ui);
    }
    if ui.inbox_open {
        return handle_inbox_key(key, ui);
    }
    if ui.context_open {
        return handle_context_key(key, ui);
    }

    let composer_empty = ui.composer.buffer().is_empty();

    // The SKILLS tab is a keyboard-owning manager: it claims the keys for its
    // list, search query, and overlays *ahead* of tab-nav and the global
    // composer. But its bare-letter/space manage hotkeys (space/e/p/n) honor
    // the deck-wide "hotkeys only from an empty composer" contract — while a
    // prompt is being typed they fall through so the letters build the prompt,
    // never trigger an edit/pin/create. Search-query typing and the modal
    // overlays are genuine text inputs and always claim. Keys it declines
    // (`None`) fall through — Tab still leaves the tab, `?` still opens help.
    if ui.tab == DeckTab::Skills
        && let Some(action) = handle_skills_key(key, ui, composer_empty)
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
                queue_issues_first_load(ui);
                return DeckAction::Handled;
            }
            KeyCode::BackTab => {
                ui.set_tab(ui.tab.prev());
                queue_issues_first_load(ui);
                return DeckAction::Handled;
            }
            KeyCode::Char('?') if composer_empty => {
                ui.help_open = true;
                ui.help_scroll = ScrollState::default();
                ui.help_scroll.follow = false;
                ui.help_scroll.top = 0;
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

    // `←` from an empty composer on the Session tab opens the SESSIONS
    // overlay — every running stella session on this machine, grouped by
    // status. `→` opens the CONTEXT overlay (this session's active skills +
    // MCP servers) without leaving the transcript. Both are gated to the
    // Session tab + full composer emptiness, so ←/→ on other tabs (Agents
    // pane nav, Graph cursor, skills pickers) are untouched, and typing a
    // prompt never trips them (handle_edit_key claims ←/→ once text exists).
    if ui.tab == DeckTab::Session && ui.composer.is_empty() {
        if matches!(key.code, KeyCode::Left) {
            return open_sessions_overlay(ui);
        }
        if matches!(key.code, KeyCode::Right) {
            return open_context_overlay(ui);
        }
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
        DeckTab::Issues => handle_issues_browse_key(key, ui, composer_empty),
        DeckTab::Session => handle_session_key(key, model, ui),
        DeckTab::Settings => handle_settings_key(key, ui, composer_empty),
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

/// The help-overlay key map. The overlay is modal: scrolling keys drive it,
/// `q`/`Esc`/`?` close it. The content is long enough to scroll on a typical
/// terminal, so a plain "any key closes" dismiss would make it unreadable.
fn handle_help_key(key: KeyEvent, ui: &mut DeckUi) -> DeckAction {
    let (total, height) = (ui.metrics.help_total, ui.metrics.help_height);
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        // Close the overlay.
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?') => {
            ui.help_open = false;
            DeckAction::Handled
        }
        // Scrolling — the same vocabulary every scrollable tab uses.
        KeyCode::Up | KeyCode::Char('k') => {
            ui.help_scroll.scroll_up(1, total, height);
            DeckAction::Handled
        }
        KeyCode::Down | KeyCode::Char('j') => {
            ui.help_scroll.scroll_down(1, total, height);
            DeckAction::Handled
        }
        KeyCode::PageUp => {
            ui.help_scroll.page_up(total, height);
            DeckAction::Handled
        }
        KeyCode::PageDown | KeyCode::Char(' ') => {
            ui.help_scroll.page_down(total, height);
            DeckAction::Handled
        }
        KeyCode::Home => {
            ui.help_scroll.to_top();
            DeckAction::Handled
        }
        KeyCode::End => {
            ui.help_scroll.to_bottom();
            DeckAction::Handled
        }
        // Ctrl-C is handled by the caller (quit precedes every modal context).
        // Any other key is swallowed so the overlay stays open and stable —
        // typing into the composer behind it would be invisible and confusing.
        _ if ctrl => DeckAction::Handled,
        _ => DeckAction::Handled,
    }
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
            // `/settings` opens the SETTINGS tab — the home of all config
            // (deck-local view state, like the tab switches above).
            "/settings" => {
                ui.set_tab(DeckTab::Settings);
                DeckAction::Handled
            }
            // The three transcript-page overlays are deck-local view state,
            // exactly like the tab switches above (their keyboard shortcuts:
            // empty-prompt `←` / `→`, and the footer's ✉ badge for the inbox).
            "/sessions" => open_sessions_overlay(ui),
            "/context" => open_context_overlay(ui),
            "/inbox" => open_inbox_overlay(ui),
            // `/mcp-search` jumps straight into the MCP tab's registry
            // search — THE way to begin looking for a server from anywhere
            // (the old `/`-on-the-MCP-tab trigger collided with the command
            // menu and is gone; `s` on the tab is the local equivalent).
            "/mcp-search" => {
                ui.set_tab(DeckTab::Mcp);
                ui.mcp.mode = McpMode::Search;
                ui.mcp.status = None;
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

/// Open the SESSIONS overlay (empty-prompt `←`, `/sessions`) and ask the
/// driver for a fresh registry snapshot.
pub(crate) fn open_sessions_overlay(ui: &mut DeckUi) -> DeckAction {
    ui.sessions_open = true;
    ui.sessions_sel = 0;
    DeckAction::Send(WorkspaceInput::SessionsRefresh)
}

/// Open the CONTEXT overlay (empty-prompt `→`, `/context`) and freshen both
/// snapshots it renders — the second refresh rides `pending_inputs` since a
/// key returns only one action.
pub(crate) fn open_context_overlay(ui: &mut DeckUi) -> DeckAction {
    ui.context_open = true;
    ui.context_scroll = 0;
    ui.pending_inputs.push(WorkspaceInput::McpRefresh);
    DeckAction::Send(WorkspaceInput::Skill(SkillOp::List))
}

/// Open the INBOX overlay (`/inbox`). The driver's poller keeps the
/// notification snapshot fresh; nothing to request.
pub(crate) fn open_inbox_overlay(ui: &mut DeckUi) -> DeckAction {
    ui.inbox_open = true;
    ui.inbox_sel = 0;
    DeckAction::Handled
}

/// The SESSIONS overlay's rows in display order: grouped by phase (the
/// [`crate::envelope::SessionPhase::ALL`] order), newest-started first within
/// a group — the flat list `sessions_sel` indexes and render walks.
pub fn grouped_session_rows(ui: &DeckUi) -> Vec<&crate::envelope::SessionInfo> {
    let mut rows = Vec::with_capacity(ui.sessions.len());
    for phase in crate::envelope::SessionPhase::ALL {
        // `Inbound::Sessions` arrives newest-started first from the driver,
        // so a stable filter keeps that order within each group.
        rows.extend(ui.sessions.iter().filter(|s| s.phase == phase));
    }
    rows
}

/// The SESSIONS overlay key map: ↑/↓ select, `⏎` resume the selected session
/// when its row is resumable (the durable-state sessions of THIS workspace
/// with no live owner) or open it read-only (replay) otherwise, `a` archive,
/// `x` delete (another session's record only — never this session's own),
/// `r` refresh, Esc/`←`/`q` close. Modal: everything else is swallowed.
fn handle_sessions_key(key: KeyEvent, ui: &mut DeckUi) -> DeckAction {
    let count = grouped_session_rows(ui).len();
    ui.sessions_sel = ui.sessions_sel.min(count.saturating_sub(1));
    match key.code {
        KeyCode::Esc | KeyCode::Left | KeyCode::Char('q') => {
            ui.sessions_open = false;
            DeckAction::Handled
        }
        KeyCode::Up => {
            ui.sessions_sel = ui.sessions_sel.saturating_sub(1);
            DeckAction::Handled
        }
        KeyCode::Down => {
            if count > 0 {
                ui.sessions_sel = (ui.sessions_sel + 1).min(count - 1);
            }
            DeckAction::Handled
        }
        KeyCode::Enter => {
            match grouped_session_rows(ui).get(ui.sessions_sel).copied() {
                // Navigate INTO the chosen session live: close the overlay
                // and hand over to the driver, which adopts the durable
                // state and continues the session in this deck (see
                // [`WorkspaceInput::SessionResume`]).
                Some(row) if row.resumable && !row.mine => {
                    let id = row.id.clone();
                    ui.sessions_open = false;
                    DeckAction::Send(WorkspaceInput::SessionResume { id })
                }
                // Every other row opens read-only: the driver registers a
                // `replay:<id>` lane and streams the persisted events (see
                // [`WorkspaceInput::SessionOpen`] — replay IS the fold). The
                // overlay closes so the replayed lane is immediately visible.
                Some(row) => {
                    let id = row.id.clone();
                    ui.sessions_open = false;
                    DeckAction::Send(WorkspaceInput::SessionOpen { id })
                }
                None => DeckAction::Handled,
            }
        }
        KeyCode::Char('r') => DeckAction::Send(WorkspaceInput::SessionsRefresh),
        KeyCode::Char('a') => match grouped_session_rows(ui).get(ui.sessions_sel).copied() {
            Some(row) => DeckAction::Send(WorkspaceInput::SessionArchive { id: row.id.clone() }),
            None => DeckAction::Handled,
        },
        KeyCode::Char('x') => {
            match grouped_session_rows(ui).get(ui.sessions_sel).copied() {
                // This deck's own record is written by this process — deleting
                // it out from under the writer would just resurrect on the
                // next transition, so the key refuses.
                Some(row) if !row.mine => {
                    DeckAction::Send(WorkspaceInput::SessionDelete { id: row.id.clone() })
                }
                _ => DeckAction::Handled,
            }
        }
        _ => DeckAction::Handled,
    }
}

/// The INBOX overlay key map: ↑/↓ select, `⏎` on a session-linked
/// notification marks it read AND opens that session (closing the overlay);
/// on an unlinked one `⏎` keeps its plain mark-read meaning. Space marks
/// read, `R` mark all, Esc/`q` close. Modal.
fn handle_inbox_key(key: KeyEvent, ui: &mut DeckUi) -> DeckAction {
    let count = ui.notifications.len();
    ui.inbox_sel = ui.inbox_sel.min(count.saturating_sub(1));
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            ui.inbox_open = false;
            DeckAction::Handled
        }
        KeyCode::Up => {
            ui.inbox_sel = ui.inbox_sel.saturating_sub(1);
            DeckAction::Handled
        }
        KeyCode::Down => {
            if count > 0 {
                ui.inbox_sel = (ui.inbox_sel + 1).min(count - 1);
            }
            DeckAction::Handled
        }
        KeyCode::Enter => match ui.notifications.get(ui.inbox_sel).cloned() {
            Some(n) => match n.session_id {
                // A session-linked notification: ⏎ marks it read (the
                // unchanged contract) AND opens the session it is about,
                // then closes the overlay. The read is the returned action;
                // the open rides `pending_inputs` — a key returns one
                // action, and the shell drains the queue right after (the
                // same mechanism the CONTEXT opener uses), so the read
                // reaches the driver first and the open follows.
                Some(session) => {
                    ui.inbox_open = false;
                    if n.read {
                        DeckAction::Send(WorkspaceInput::SessionOpen { id: session })
                    } else {
                        ui.pending_inputs
                            .push(WorkspaceInput::SessionOpen { id: session });
                        DeckAction::Send(WorkspaceInput::NotificationRead { id: n.id })
                    }
                }
                // No session link: exactly the pre-existing mark-read
                // behavior (a no-op once read), overlay stays open.
                None if !n.read => DeckAction::Send(WorkspaceInput::NotificationRead { id: n.id }),
                None => DeckAction::Handled,
            },
            None => DeckAction::Handled,
        },
        KeyCode::Char(' ') => match ui.notifications.get(ui.inbox_sel) {
            Some(n) if !n.read => {
                DeckAction::Send(WorkspaceInput::NotificationRead { id: n.id.clone() })
            }
            _ => DeckAction::Handled,
        },
        KeyCode::Char('R') => DeckAction::Send(WorkspaceInput::NotificationsReadAll),
        _ => DeckAction::Handled,
    }
}

/// The CONTEXT overlay key map: ↑/↓/PageUp/PageDown scroll, Esc/`→`/`q`
/// close. Read-only — management lives on the SKILLS/MCP tabs. Modal.
fn handle_context_key(key: KeyEvent, ui: &mut DeckUi) -> DeckAction {
    match key.code {
        KeyCode::Esc | KeyCode::Right | KeyCode::Char('q') => {
            ui.context_open = false;
            DeckAction::Handled
        }
        KeyCode::Up => {
            ui.context_scroll = ui.context_scroll.saturating_sub(1);
            DeckAction::Handled
        }
        KeyCode::Down => {
            // Render clamps to the content height it measures.
            ui.context_scroll = ui.context_scroll.saturating_add(1);
            DeckAction::Handled
        }
        KeyCode::PageUp => {
            ui.context_scroll = ui.context_scroll.saturating_sub(10);
            DeckAction::Handled
        }
        KeyCode::PageDown => {
            ui.context_scroll = ui.context_scroll.saturating_add(10);
            DeckAction::Handled
        }
        _ => DeckAction::Handled,
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

// ── ISSUES tab ──────────────────────────────────────────────────────────────

/// First visit to the ISSUES tab loads the list without a keypress — the
/// same first-visit affordance as the INSTALLED AGENTS pane. Rides
/// [`DeckUi::pending_inputs`] because the tab switch itself already is the
/// key's returned action.
fn queue_issues_first_load(ui: &mut DeckUi) {
    if ui.tab == DeckTab::Issues && !ui.issues.loaded && !ui.issues.busy {
        let seq = ui.issues.bump_seq();
        ui.issues.list_wait = seq;
        ui.issues.busy = true;
        ui.issues.notice = Some("loading issues…".into());
        ui.pending_inputs.push(WorkspaceInput::IssuesRefresh {
            query: None,
            state: None,
            seq,
        });
    }
}

/// The ISSUES sub-modes' keys, dispatched by the modal gate in
/// [`handle_deck_key`]. Every key is consumed — nothing leaks to the
/// composer or the tab views while a sub-mode is open.
fn handle_issues_modal_key(key: KeyEvent, ui: &mut DeckUi) -> DeckAction {
    match ui.issues.mode {
        IssuesMode::SearchTracker => handle_issues_search_key(key, ui),
        IssuesMode::Create => handle_issue_form_key(key, ui),
        IssuesMode::Comment | IssuesMode::SetStatus => handle_issue_prompt_key(key, ui),
        // Unreachable — the gate only fires for non-Browse modes.
        IssuesMode::Browse => DeckAction::Ignored,
    }
}

/// The tracker-search query line (`/`): type the query, ⏎ fires the
/// tracker-side search, Esc returns to browse.
fn handle_issues_search_key(key: KeyEvent, ui: &mut DeckUi) -> DeckAction {
    match key.code {
        KeyCode::Esc => {
            ui.issues.mode = IssuesMode::Browse;
            DeckAction::Handled
        }
        KeyCode::Enter => {
            let query = ui.issues.search_query.trim().to_string();
            let seq = ui.issues.bump_seq();
            ui.issues.list_wait = seq;
            ui.issues.busy = true;
            ui.issues.mode = IssuesMode::Browse;
            ui.issues.notice = Some(if query.is_empty() {
                "refreshing…".to_string()
            } else {
                format!("searching “{query}”…")
            });
            DeckAction::Send(WorkspaceInput::IssuesRefresh {
                query: (!query.is_empty()).then_some(query),
                state: None,
                seq,
            })
        }
        KeyCode::Backspace => {
            ui.issues.search_query.pop();
            DeckAction::Handled
        }
        KeyCode::Char(c)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER | KeyModifiers::META) =>
        {
            ui.issues.search_query.push(c);
            DeckAction::Handled
        }
        _ => DeckAction::Handled,
    }
}

/// The one-line Comment / SetStatus prompts: type, ⏎ dispatches the
/// [`WorkspaceInput::IssueAct`] for the selected issue, Esc cancels.
fn handle_issue_prompt_key(key: KeyEvent, ui: &mut DeckUi) -> DeckAction {
    match key.code {
        KeyCode::Esc => {
            ui.issues.mode = IssuesMode::Browse;
            ui.issues.input.clear();
            DeckAction::Handled
        }
        KeyCode::Enter => {
            let text = ui.issues.input.trim().to_string();
            if text.is_empty() {
                ui.issues.notice = Some(match ui.issues.mode {
                    IssuesMode::Comment => "type the comment first".into(),
                    _ => "type a status word first".into(),
                });
                return DeckAction::Handled;
            }
            let Some(row) = ui.issues.selected() else {
                ui.issues.mode = IssuesMode::Browse;
                return DeckAction::Handled;
            };
            let issue_key = row.key.clone();
            let (action, notice) = match ui.issues.mode {
                IssuesMode::Comment => (
                    IssueAction::Comment(text),
                    format!("commenting on {issue_key}…"),
                ),
                _ => (
                    IssueAction::SetStatus(text.clone()),
                    format!("setting {issue_key} → {text}…"),
                ),
            };
            let seq = ui.issues.bump_seq();
            ui.issues.act_wait = seq;
            ui.issues.busy = true;
            ui.issues.input.clear();
            ui.issues.mode = IssuesMode::Browse;
            ui.issues.notice = Some(notice);
            DeckAction::Send(WorkspaceInput::IssueAct {
                key: issue_key,
                action,
                seq,
            })
        }
        KeyCode::Backspace => {
            ui.issues.input.pop();
            DeckAction::Handled
        }
        KeyCode::Char(c)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER | KeyModifiers::META) =>
        {
            ui.issues.input.push(c);
            DeckAction::Handled
        }
        _ => DeckAction::Handled,
    }
}

/// Validate + submit the create form. Title is required; labels split on
/// commas; a blank assignee means unassigned. The driver answers with
/// [`Inbound::IssueActDone`] and then a refreshed [`Inbound::IssuesList`]
/// under the same seq, so both wait lanes arm here.
fn submit_issue_form(ui: &mut DeckUi) -> DeckAction {
    let title = ui.issues.form_title.trim().to_string();
    if title.is_empty() {
        ui.issues.notice = Some("the issue needs a title".into());
        ui.issues.form_field = IssueField::Title;
        return DeckAction::Handled;
    }
    let body = ui.issues.form_body.buffer().to_string();
    let labels: Vec<String> = ui
        .issues
        .form_labels
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    let assignee = {
        let a = ui.issues.form_assignee.trim();
        (!a.is_empty()).then(|| a.to_string())
    };
    let seq = ui.issues.bump_seq();
    ui.issues.act_wait = seq;
    ui.issues.list_wait = seq;
    ui.issues.busy = true;
    ui.issues.clear_form();
    ui.issues.mode = IssuesMode::Browse;
    ui.issues.notice = Some(format!("creating “{title}”…"));
    DeckAction::Send(WorkspaceInput::IssueCreate {
        title,
        body,
        labels,
        assignee,
        seq,
    })
}

/// The create form's keys. While the type-ahead popup is open it owns
/// ↑/↓/Enter/Tab/Esc; every other key keeps editing the active field (and,
/// on an entity field, re-fires the per-keystroke search).
fn handle_issue_form_key(key: KeyEvent, ui: &mut DeckUi) -> DeckAction {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    if ui.issues.typeahead.open() {
        match key.code {
            KeyCode::Esc => {
                // Close the popup, keep the typed text; a second Esc (popup
                // now closed) cancels the whole form below.
                ui.issues.typeahead.close();
                return DeckAction::Handled;
            }
            KeyCode::Up => {
                ui.issues.typeahead.sel = ui.issues.typeahead.sel.saturating_sub(1);
                return DeckAction::Handled;
            }
            KeyCode::Down => {
                let n = ui.issues.typeahead.hits.len();
                if n > 0 {
                    ui.issues.typeahead.sel = (ui.issues.typeahead.sel + 1).min(n - 1);
                }
                return DeckAction::Handled;
            }
            KeyCode::Enter | KeyCode::Tab => {
                let picked = ui
                    .issues
                    .typeahead
                    .hits
                    .get(ui.issues.typeahead.sel)
                    .cloned();
                if let Some(hit) = picked
                    && let Some((text, field)) = ui.issues.entity_text_mut()
                {
                    apply_entity_insert(text, field, &hit.insert);
                }
                ui.issues.typeahead.close();
                return DeckAction::Handled;
            }
            _ => {}
        }
    }

    match key.code {
        KeyCode::Esc => {
            ui.issues.clear_form();
            ui.issues.mode = IssuesMode::Browse;
            ui.issues.notice = Some("create cancelled".into());
            DeckAction::Handled
        }
        // Submit from anywhere in the form — the agent editor's ctrl+s idiom.
        KeyCode::Char('s') if ctrl => submit_issue_form(ui),
        KeyCode::Tab => {
            ui.issues.typeahead.close();
            ui.issues.form_field = ui.issues.form_field.next();
            DeckAction::Handled
        }
        KeyCode::BackTab => {
            ui.issues.typeahead.close();
            ui.issues.form_field = ui.issues.form_field.prev();
            DeckAction::Handled
        }
        // ↑/↓ cycle fields where that is unambiguous — the multi-line body
        // keeps them for cursor motion (handled by the fallthrough below).
        KeyCode::Up if ui.issues.form_field != IssueField::Body => {
            ui.issues.typeahead.close();
            ui.issues.form_field = ui.issues.form_field.prev();
            DeckAction::Handled
        }
        KeyCode::Down if ui.issues.form_field != IssueField::Body => {
            ui.issues.typeahead.close();
            ui.issues.form_field = ui.issues.form_field.next();
            DeckAction::Handled
        }
        KeyCode::Enter => match ui.issues.form_field {
            IssueField::Title => {
                ui.issues.form_field = IssueField::Body;
                DeckAction::Handled
            }
            // A textarea's Enter is a line break, never a submit.
            IssueField::Body => {
                ui.issues.form_body.insert_newline();
                DeckAction::Handled
            }
            IssueField::Labels => {
                ui.issues.form_field = IssueField::Assignee;
                DeckAction::Handled
            }
            // The last field: ⏎ with the popup closed submits.
            IssueField::Assignee => submit_issue_form(ui),
        },
        KeyCode::Backspace => {
            match ui.issues.form_field {
                IssueField::Title => {
                    ui.issues.form_title.pop();
                }
                IssueField::Body => ui.issues.form_body.backspace(),
                IssueField::Labels | IssueField::Assignee => {
                    if let Some((text, _)) = ui.issues.entity_text_mut() {
                        text.pop();
                    }
                    if let Some(input) = typeahead_after_edit(&mut ui.issues) {
                        ui.pending_inputs.push(input);
                    }
                }
            }
            DeckAction::Handled
        }
        KeyCode::Char(c)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER | KeyModifiers::META) =>
        {
            match ui.issues.form_field {
                IssueField::Title => ui.issues.form_title.push(c),
                IssueField::Body => ui.issues.form_body.insert_char(c),
                // THE type-ahead contract: the popup opens the instant the
                // first character lands (`@` included) and every edit
                // re-fires the search.
                IssueField::Labels | IssueField::Assignee => {
                    if let Some((text, _)) = ui.issues.entity_text_mut() {
                        text.push(c);
                    }
                    if let Some(input) = typeahead_after_edit(&mut ui.issues) {
                        ui.pending_inputs.push(input);
                    }
                }
            }
            DeckAction::Handled
        }
        _ => {
            // Cursor motion inside the multi-line body; everything else is
            // swallowed — the form is modal.
            if ui.issues.form_field == IssueField::Body {
                let _ = handle_edit_key(key, &mut ui.issues.form_body);
            }
            DeckAction::Handled
        }
    }
}

/// The ISSUES tab's browse keys (non-modal — the composer stays live, so
/// every letter verb is gated on a blank composer, exactly like the MCP
/// tab): ↑/↓ select · `r` refresh · `/` tracker search · `n` create ·
/// `c` comment · `s` set status · `w` start work.
fn handle_issues_browse_key(
    key: KeyEvent,
    ui: &mut DeckUi,
    composer_empty: bool,
) -> Option<DeckAction> {
    let count = ui.issues.rows.len();
    match key.code {
        KeyCode::Up => {
            ui.issues.sel = ui.issues.sel.saturating_sub(1);
            Some(DeckAction::Handled)
        }
        KeyCode::Down => {
            if count > 0 {
                ui.issues.sel = (ui.issues.sel + 1).min(count - 1);
            }
            Some(DeckAction::Handled)
        }
        KeyCode::Char('r') if composer_empty => {
            let seq = ui.issues.bump_seq();
            ui.issues.list_wait = seq;
            ui.issues.busy = true;
            ui.issues.notice = Some("refreshing…".into());
            Some(DeckAction::Send(WorkspaceInput::IssuesRefresh {
                query: None,
                state: None,
                seq,
            }))
        }
        KeyCode::Char('/') if composer_empty => {
            ui.issues.mode = IssuesMode::SearchTracker;
            ui.issues.search_query.clear();
            Some(DeckAction::Handled)
        }
        KeyCode::Char('n') if composer_empty => {
            ui.issues.clear_form();
            ui.issues.mode = IssuesMode::Create;
            ui.issues.notice = None;
            Some(DeckAction::Handled)
        }
        KeyCode::Char('c') if composer_empty => {
            if ui.issues.selected().is_some() {
                ui.issues.input.clear();
                ui.issues.mode = IssuesMode::Comment;
            } else {
                ui.issues.notice = Some("no issue selected — r loads the list".into());
            }
            Some(DeckAction::Handled)
        }
        KeyCode::Char('s') if composer_empty => {
            if ui.issues.selected().is_some() {
                ui.issues.input.clear();
                ui.issues.mode = IssuesMode::SetStatus;
            } else {
                ui.issues.notice = Some("no issue selected — r loads the list".into());
            }
            Some(DeckAction::Handled)
        }
        KeyCode::Char('w') if composer_empty => {
            let Some(row) = ui.issues.selected() else {
                ui.issues.notice = Some("no issue selected — r loads the list".into());
                return Some(DeckAction::Handled);
            };
            let issue_key = row.key.clone();
            let seq = ui.issues.bump_seq();
            ui.issues.act_wait = seq;
            ui.issues.busy = true;
            ui.issues.notice = Some(format!("starting work on {issue_key}…"));
            Some(DeckAction::Send(WorkspaceInput::IssueAct {
                key: issue_key,
                action: IssueAction::StartWork,
                seq,
            }))
        }
        _ => None,
    }
}

/// SKILLS-tab keys. Returns `Some` for keys the tab claims ahead of the
/// composer (nav, manage hotkeys, the search query, overlays), `None` for keys
/// that should fall through to the deck-global handlers — Tab still leaves the
/// tab and `?` still opens help from the installed pane.
fn handle_skills_key(key: KeyEvent, ui: &mut DeckUi, composer_empty: bool) -> Option<DeckAction> {
    // The ctrl+o preview overlay is fully modal (scroll + esc close) and sits
    // ahead of every pane and the other prompts.
    if ui.skills.preview.is_some() {
        return Some(handle_skills_preview_key(key, ui));
    }
    // An overlay (scope picker / create / edit / pin) is fully modal.
    if ui.skills.prompt.is_some() {
        return Some(handle_skills_prompt_key(key, ui));
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match ui.skills.focus {
        SkillsFocus::Installed => handle_skills_installed_key(key, ui, ctrl, composer_empty),
        SkillsFocus::Search => handle_skills_search_key(key, ui),
    }
}

/// Open the ctrl+o preview for the highlighted installed skill — its body is
/// already in hand (`SkillRow::body`), so no driver round-trip.
fn open_installed_preview(ui: &mut DeckUi) -> Option<DeckAction> {
    let row = ui.skills.view.rows.get(ui.skills.sel)?;
    ui.skills.preview = Some(SkillPreview {
        title: row.name.clone(),
        subtitle: format!("{} · {} · v{}", row.scope.label(), row.origin, row.version),
        pending: None,
        body: Some(if row.body.trim().is_empty() {
            "*(this skill has an empty body)*".to_string()
        } else {
            row.body.clone()
        }),
        scroll: 0,
    });
    Some(DeckAction::Handled)
}

/// Open the ctrl+o preview for the highlighted registry hit — the body is not
/// local, so show a loading state and ask the driver to fetch the `SKILL.md`.
fn open_search_preview(ui: &mut DeckUi) -> Option<DeckAction> {
    if ui.skills.hits.is_empty() {
        return Some(DeckAction::Handled);
    }
    let idx = ui.skills.search_sel.min(ui.skills.hits.len() - 1);
    let hit = &ui.skills.hits[idx];
    let id = hit.id.clone();
    ui.skills.preview = Some(SkillPreview {
        title: hit.id.clone(),
        subtitle: if hit.url.is_empty() {
            hit.installs.clone()
        } else {
            hit.url.clone()
        },
        pending: Some(id.clone()),
        body: None,
        scroll: 0,
    });
    Some(DeckAction::Send(WorkspaceInput::Skill(SkillOp::Preview {
        id,
    })))
}

/// The preview overlay keys (fully modal): scroll the body (↑/↓, PageUp/Down,
/// Home/End) and dismiss (esc / ctrl+o / q).
fn handle_skills_preview_key(key: KeyEvent, ui: &mut DeckUi) -> DeckAction {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let Some(preview) = ui.skills.preview.as_mut() else {
        return DeckAction::Handled;
    };
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            ui.skills.preview = None;
        }
        KeyCode::Char('o') if ctrl => {
            ui.skills.preview = None;
        }
        KeyCode::Up | KeyCode::Char('k') => {
            preview.scroll = preview.scroll.saturating_sub(1);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            // Upper bound is clamped to real content height at render time.
            preview.scroll = preview.scroll.saturating_add(1);
        }
        KeyCode::PageUp => {
            preview.scroll = preview.scroll.saturating_sub(10);
        }
        KeyCode::PageDown | KeyCode::Char(' ') => {
            preview.scroll = preview.scroll.saturating_add(10);
        }
        KeyCode::Home => {
            preview.scroll = 0;
        }
        KeyCode::End => {
            // A large value; render clamps it down to the last page.
            preview.scroll = u16::MAX;
        }
        _ => {}
    }
    DeckAction::Handled
}

/// The installed-skills (manage) pane: navigate, toggle enabled (space),
/// uninstall (ctrl+x twice), edit (e), pin (p), create (n), cross to search (→).
fn handle_skills_installed_key(
    key: KeyEvent,
    ui: &mut DeckUi,
    ctrl: bool,
    composer_empty: bool,
) -> Option<DeckAction> {
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
        // space toggles enabled — but only from an empty composer, so a space
        // typed mid-prompt builds the prompt instead of flipping a skill.
        KeyCode::Char(' ') if composer_empty => {
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
        // Preview the selected skill's rendered SKILL.md (scrollable, esc closes).
        KeyCode::Char('o') if ctrl => open_installed_preview(ui),
        // Edit the selected skill's body (saving makes a new pinned version).
        KeyCode::Char('e') if !ctrl && composer_empty => {
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
        KeyCode::Char('p') if !ctrl && composer_empty => {
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
        KeyCode::Char('n') if !ctrl && composer_empty => {
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
        // Preview the highlighted hit's rendered SKILL.md — fetched by the
        // driver, shown scrollable (esc closes).
        KeyCode::Char('o') if ctrl => open_search_preview(ui),
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
                if let Some(submission) = ui.composer.take_submission() {
                    return Some(DeckAction::Send(WorkspaceInput::ToAgent {
                        agent: agent.clone(),
                        input: UserInput::AskUserAnswer {
                            id: prompt.id.clone(),
                            answer: submission.text,
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
        // Enter registry-search mode. `s` sits with the tab's other letter
        // actions (e/a/x/r, all gated on an empty composer). `/` deliberately
        // does NOT enter search anymore: it belongs to the command menu
        // everywhere — `/mcp-search` in that menu lands here too.
        KeyCode::Char('s') if composer_empty => {
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
        // Start the browser OAuth login for the selected server. Http-only —
        // a stdio server has no authorization server, so the key explains
        // instead of firing.
        KeyCode::Char('o') if composer_empty => {
            let server = ui.mcp.selected_server()?;
            let name = server.name.clone();
            if server.oauth.is_none() {
                ui.mcp.status = Some(format!(
                    "{name}: OAuth login applies to http servers (use `a` for env credentials)"
                ));
                return Some(DeckAction::Handled);
            }
            ui.mcp.status = Some(format!("{name}: starting OAuth login…"));
            Some(DeckAction::Send(WorkspaceInput::McpOauthLogin {
                server: name,
            }))
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
                        // Drop back to the Browse list so the refreshed
                        // installed-servers snapshot (pushed once the install
                        // lands) is actually on screen — Search mode would
                        // otherwise hide it behind now-stale results.
                        ui.mcp.mode = McpMode::Browse;
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

/// The SETTINGS tab's browse keys (non-modal — the composer stays live). The
/// tab hosts the `agent_engine_config` editor; `e` hands it the keyboard (its
/// own Esc hands it back), gated on a blank composer like every other tab's
/// letter verb so typing a prompt still works from here. Once focused, the
/// editor claims every key ahead of this handler (see `handle_deck_key`).
fn handle_settings_key(key: KeyEvent, ui: &mut DeckUi, composer_empty: bool) -> Option<DeckAction> {
    if composer_empty && key.modifiers.is_empty() && matches!(key.code, KeyCode::Char('e')) {
        return Some(crate::views::engine::focus_panel(ui));
    }
    None
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
        // `s` stop · `p` pause/resume toggle (by the row's current status) ·
        // `r` restart. The driver honors all three on worker lanes
        // (`req:`/`sub:`): pause parks the worker at its next step boundary
        // (never mid-tool), restart respawns the lane from its retained
        // spec. On the lead they are no-ops (Esc is the lead's interrupt).
        KeyCode::Char('s') if composer_empty => model.agents.get(ui.focused).map(|entry| {
            DeckAction::Send(WorkspaceInput::Control {
                agent: entry.meta.id.clone(),
                control: AgentControl::Stop,
            })
        }),
        KeyCode::Char('p') if composer_empty => model.agents.get(ui.focused).map(|entry| {
            DeckAction::Send(WorkspaceInput::Control {
                agent: entry.meta.id.clone(),
                control: if entry.status == AgentStatus::Paused {
                    AgentControl::Resume
                } else {
                    AgentControl::Pause
                },
            })
        }),
        KeyCode::Char('r') if composer_empty => model.agents.get(ui.focused).map(|entry| {
            DeckAction::Send(WorkspaceInput::Control {
                agent: entry.meta.id.clone(),
                control: AgentControl::Restart,
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
            KeyCode::PageUp => {
                ui.files_diff_scroll.scroll_up(height.max(1), total, height);
                return Some(DeckAction::Handled);
            }
            KeyCode::PageDown => {
                ui.files_diff_scroll
                    .scroll_down(height.max(1), total, height);
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
    // ⌘/⌃ + [ / ] jump to the transcript's ends (the composer's ⌥[ / ⌥]
    // cursor motion is untouched — different modifier). On terminals without
    // the kitty keyboard protocol ⌃[ arrives as Esc and simply follows the
    // Esc rules instead; nothing surprising happens.
    let jump_mod = key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER | KeyModifiers::META);
    match key.code {
        // Jump to the beginning of the session's transcript…
        KeyCode::Char('[') if jump_mod => {
            ui.session_selected = None;
            ui.session_scroll.to_top();
            return Some(DeckAction::Handled);
        }
        // …and to the end (which re-arms tail-follow).
        KeyCode::Char(']') if jump_mod => {
            ui.session_selected = None;
            ui.session_scroll.to_bottom();
            return Some(DeckAction::Handled);
        }
        _ => {}
    }
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
        // The expand-ALL overlay's Esc way out (precedence rule 8): claimed
        // here, ahead of the turn-stop Esc, so closing the overlay can never
        // cancel a running turn.
        KeyCode::Esc if ui.transcript_expand_all => {
            ui.transcript_expand_all = false;
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
    // The deck queue is text-shaped: attachments enter deck prompts as
    // pasted payload *paths* (extracted at dispatch by the driver), so the
    // submission's text is the whole content here.
    match ui.composer.take_submission().map(|s| s.text) {
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
        KeyCode::Enter => match ui.composer.take_submission().map(|s| s.text) {
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
                speculated: false,
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
    fn ctrl_o_with_no_selection_toggles_the_expand_all_overlay() {
        let mut model = model_with(&["lead"]);
        with_tool_exchange(&mut model, "lead");
        let mut ui = ready_ui();

        // First press (no selection): every expandable message opens at once —
        // no per-entry set is touched.
        handle_deck_key(ctrl('o'), &model, &mut ui);
        assert!(ui.transcript_expand_all, "expand-all overlay on");
        assert!(
            ui.expanded.get("lead").is_none_or(|set| set.is_empty()),
            "the overlay does not write the per-entry sets"
        );
        // Second press: everything closes again — ctrl+o is its own way out.
        handle_deck_key(ctrl('o'), &model, &mut ui);
        assert!(!ui.transcript_expand_all, "ctrl+o again collapses");
    }

    #[test]
    fn esc_collapses_the_expand_all_overlay_before_stopping_the_turn() {
        let mut model = model_with(&["lead"]);
        with_tool_exchange(&mut model, "lead"); // events flip the agent to Running
        let mut ui = ready_ui();
        handle_deck_key(ctrl('o'), &model, &mut ui);
        assert!(ui.transcript_expand_all);

        // Esc's first job here is the overlay — NOT cancelling the running
        // turn (precedence rule 8 beats rules 9–10).
        let action = handle_deck_key(key(KeyCode::Esc), &model, &mut ui);
        assert!(!ui.transcript_expand_all, "esc is a graceful way out");
        assert!(
            matches!(action, DeckAction::Handled),
            "the overlay-collapsing esc must not reach the turn-stop rules"
        );
        // With the overlay gone, the next Esc resumes normal duty (stop the
        // in-flight turn).
        let action = handle_deck_key(key(KeyCode::Esc), &model, &mut ui);
        assert!(
            matches!(action, DeckAction::Send(WorkspaceInput::Control { .. })),
            "esc after the overlay closes stops the turn as before"
        );
    }

    #[test]
    fn ctrl_o_on_a_highlight_peels_one_row_out_of_the_expand_all_overlay() {
        let mut model = model_with(&["lead"]);
        with_tool_exchange(&mut model, "lead"); // entries 0 (call) + 1 (result)
        let mut ui = ready_ui();
        handle_deck_key(ctrl('o'), &model, &mut ui); // overlay on
        handle_deck_key(key(KeyCode::Up), &model, &mut ui); // highlight entry 1

        handle_deck_key(ctrl('o'), &model, &mut ui);
        assert!(
            !ui.transcript_expand_all,
            "the overlay materializes into per-entry expansions"
        );
        let set = ui.expanded.get("lead").expect("materialized set");
        assert!(
            set.contains(&0) && !set.contains(&1),
            "the highlighted row collapsed; the rest stay open: {set:?}"
        );
    }

    #[test]
    fn bracket_jumps_reach_both_ends_of_the_transcript() {
        let mut model = model_with(&["lead"]);
        with_tool_exchange(&mut model, "lead");
        let mut ui = ready_ui();
        // A scrollable transcript (metrics as the render pass would set them).
        ui.metrics.session_total = 100;
        ui.metrics.session_height = 10;

        // ⌘/⌃ [ pins the window to the very beginning of the session…
        handle_deck_key(ctrl('['), &model, &mut ui);
        assert!(!ui.session_scroll.follow);
        assert_eq!(ui.session_scroll.window(100, 10), 0..10, "jumped to start");

        // …and ⌘/⌃ ] returns to the end, re-arming tail-follow. Both also
        // drop any highlight so the pinned selection can't yank the view back.
        let cmd_close = KeyEvent::new(KeyCode::Char(']'), KeyModifiers::SUPER);
        handle_deck_key(cmd_close, &model, &mut ui);
        assert!(ui.session_scroll.follow, "jumped to end = follow the tail");
        assert_eq!(ui.session_selected, None);
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

    #[test]
    fn deregister_keeps_focus_in_bounds_and_on_the_same_agent() {
        let mut model = model_with(&["lead", "req:1", "req:2"]);
        let mut ui = ready_ui();
        ui.focused = 2; // req:2

        // An EARLIER row vanishing shifts indexes down: the focused AGENT
        // stays focused at its new index.
        ingest_inbound(
            &Inbound::Deregister {
                agent: "req:1".into(),
            },
            &mut model,
            &mut ui,
        );
        assert_eq!(ui.focused, 1);
        assert_eq!(model.agents[ui.focused].meta.id, "req:2");

        // The focused LAST row vanishing clamps focus back into range.
        ingest_inbound(
            &Inbound::Deregister {
                agent: "req:2".into(),
            },
            &mut model,
            &mut ui,
        );
        assert_eq!(ui.focused, 0, "focus stays in bounds");
        assert_eq!(model.agents[ui.focused].meta.id, "lead");
    }

    #[test]
    fn deregister_of_the_focused_row_drops_the_stale_selection() {
        let mut model = model_with(&["lead", "req:1", "req:2"]);
        with_tool_exchange(&mut model, "req:1");
        with_tool_exchange(&mut model, "req:2");
        let mut ui = ready_ui();
        ui.focused = 1; // req:1
        ui.session_selected = Some(1); // a row of req:1's transcript

        ingest_inbound(
            &Inbound::Deregister {
                agent: "req:1".into(),
            },
            &mut model,
            &mut ui,
        );
        // The successor (req:2) slides into the focused index…
        assert_eq!(ui.focused, 1);
        assert_eq!(model.agents[1].meta.id, "req:2");
        // …but the selection indexed the REMOVED agent's transcript, so it
        // must not re-attach to the successor's (which is long enough that
        // range-clamping alone would have kept it).
        assert_eq!(ui.session_selected, None);
        assert!(ui.session_scroll.follow, "tail-follow re-arms");
    }

    fn ready_ui() -> DeckUi {
        let mut ui = DeckUi::default();
        ui.splash.skip(); // past the splash for interaction tests
        ui
    }

    fn session_info(id: &str) -> crate::envelope::SessionInfo {
        crate::envelope::SessionInfo {
            id: id.into(),
            title: format!("title for {id}"),
            summary: String::new(),
            workspace: "/tmp/w".into(),
            phase: crate::envelope::SessionPhase::Complete,
            started_ms: 0,
            updated_ms: 0,
            mine: false,
            resumable: false,
        }
    }

    fn notification(
        id: &str,
        read: bool,
        session: Option<&str>,
    ) -> crate::envelope::NotificationInfo {
        crate::envelope::NotificationInfo {
            id: id.into(),
            title: "a title".into(),
            body: "a body".into(),
            source: String::new(),
            created_ms: 0,
            read,
            session_id: session.map(str::to_string),
        }
    }

    #[test]
    fn sessions_overlay_enter_opens_the_selected_session_and_closes() {
        let model = model_with(&["lead"]);
        let mut ui = ready_ui();
        ui.sessions_open = true;
        ui.sessions = vec![session_info("ses-1"), session_info("ses-2")];
        ui.sessions_sel = 1;

        let action = handle_deck_key(key(KeyCode::Enter), &model, &mut ui);
        assert_eq!(
            action,
            DeckAction::Send(WorkspaceInput::SessionOpen { id: "ses-2".into() }),
            "⏎ opens (replays) the selected session"
        );
        assert!(!ui.sessions_open, "the overlay closes on open");
    }

    #[test]
    fn sessions_overlay_enter_with_no_rows_is_a_no_op() {
        let model = model_with(&["lead"]);
        let mut ui = ready_ui();
        ui.sessions_open = true; // registry snapshot empty

        let action = handle_deck_key(key(KeyCode::Enter), &model, &mut ui);
        assert_eq!(action, DeckAction::Handled, "nothing to open");
        assert!(ui.sessions_open, "the overlay stays up (Esc closes it)");
    }

    #[test]
    fn inbox_enter_on_a_linked_notification_marks_read_and_opens_the_session() {
        let model = model_with(&["lead"]);
        let mut ui = ready_ui();
        ui.inbox_open = true;
        ui.notifications = vec![notification("n1", false, Some("ses-9"))];

        let action = handle_deck_key(key(KeyCode::Enter), &model, &mut ui);
        assert_eq!(
            action,
            DeckAction::Send(WorkspaceInput::NotificationRead { id: "n1".into() }),
            "the read goes out as the key's action"
        );
        assert_eq!(
            ui.pending_inputs,
            vec![WorkspaceInput::SessionOpen { id: "ses-9".into() }],
            "…and the open rides pending_inputs right behind it"
        );
        assert!(!ui.inbox_open, "the overlay closes when a session opens");
    }

    #[test]
    fn inbox_enter_on_an_already_read_linked_notification_just_opens() {
        let model = model_with(&["lead"]);
        let mut ui = ready_ui();
        ui.inbox_open = true;
        ui.notifications = vec![notification("n1", true, Some("ses-9"))];

        let action = handle_deck_key(key(KeyCode::Enter), &model, &mut ui);
        assert_eq!(
            action,
            DeckAction::Send(WorkspaceInput::SessionOpen { id: "ses-9".into() }),
            "already read — no second NotificationRead, just the open"
        );
        assert!(ui.pending_inputs.is_empty());
        assert!(!ui.inbox_open);
    }

    #[test]
    fn inbox_enter_without_a_session_link_keeps_the_mark_read_behavior() {
        let model = model_with(&["lead"]);
        let mut ui = ready_ui();
        ui.inbox_open = true;
        ui.notifications = vec![notification("n1", false, None)];

        let action = handle_deck_key(key(KeyCode::Enter), &model, &mut ui);
        assert_eq!(
            action,
            DeckAction::Send(WorkspaceInput::NotificationRead { id: "n1".into() }),
            "unlinked ⏎ is exactly the old mark-read"
        );
        assert!(ui.pending_inputs.is_empty(), "no session to open");
        assert!(ui.inbox_open, "the overlay stays open, as before");

        // Once read, ⏎ on an unlinked notification is a no-op.
        ui.notifications = vec![notification("n1", true, None)];
        let action = handle_deck_key(key(KeyCode::Enter), &model, &mut ui);
        assert_eq!(action, DeckAction::Handled);
        assert!(ui.inbox_open);
    }

    #[test]
    fn inbox_space_only_marks_read_and_never_opens() {
        let model = model_with(&["lead"]);
        let mut ui = ready_ui();
        ui.inbox_open = true;
        ui.notifications = vec![notification("n1", false, Some("ses-9"))];

        let action = handle_deck_key(key(KeyCode::Char(' ')), &model, &mut ui);
        assert_eq!(
            action,
            DeckAction::Send(WorkspaceInput::NotificationRead { id: "n1".into() }),
            "␣ keeps its plain mark-read meaning"
        );
        assert!(ui.pending_inputs.is_empty(), "␣ never opens the session");
        assert!(ui.inbox_open, "␣ never closes the overlay");
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
    fn splash_cues_replay_and_release_the_cinematic() {
        let mut model = WorkspaceModel::new();
        let mut ui = DeckUi::default();
        ui.splash.skip(); // the deck is up; `/init` arrives later
        assert!(ui.splash.is_done());

        // Replay: a fresh held splash owns the frame again…
        ingest_inbound(&Inbound::Splash(SplashCue::Replay), &mut model, &mut ui);
        assert!(!ui.splash.is_done(), "replay restarts the cinematic");

        // …and Release is what lets its timeline run out (exact timing is
        // splash::tests territory — here we only pin that the cue routes).
        ingest_inbound(&Inbound::Splash(SplashCue::Release), &mut model, &mut ui);
        assert!(!ui.splash.is_done(), "the battle floor still plays out");
    }

    #[test]
    fn no_anim_sessions_ignore_splash_replays() {
        let mut model = WorkspaceModel::new();
        let mut ui = DeckUi::default();
        ui.no_anim = true;
        ui.splash.skip();
        ingest_inbound(&Inbound::Splash(SplashCue::Replay), &mut model, &mut ui);
        assert!(
            ui.splash.is_done(),
            "a no-anim session never replays the cinematic"
        );
    }

    fn session_row(
        id: &str,
        phase: crate::envelope::SessionPhase,
        mine: bool,
        resumable: bool,
    ) -> crate::envelope::SessionInfo {
        crate::envelope::SessionInfo {
            id: id.into(),
            title: format!("ws: {id}"),
            summary: String::new(),
            workspace: "/w".into(),
            phase,
            started_ms: 0,
            updated_ms: 0,
            mine,
            resumable,
        }
    }

    #[test]
    fn sessions_overlay_enter_resumes_resumable_rows_and_opens_the_rest() {
        use crate::envelope::SessionPhase;
        let model = model_with(&["lead"]);
        let mut ui = ready_ui();
        ui.sessions_open = true;
        ui.sessions = vec![
            session_row("ses-mine", SessionPhase::InProgress, true, false),
            session_row("ses-paused", SessionPhase::Paused, false, true),
            session_row("ses-foreign", SessionPhase::Complete, false, false),
        ];

        // Grouped order: InProgress (mine) · Paused (resumable) · Complete.
        // ⏎ on the resumable row navigates into it LIVE: the overlay closes
        // and the driver is told to resume exactly that session.
        ui.sessions_sel = 1;
        assert_eq!(
            handle_deck_key(key(KeyCode::Enter), &model, &mut ui),
            DeckAction::Send(WorkspaceInput::SessionResume {
                id: "ses-paused".into()
            })
        );
        assert!(!ui.sessions_open, "the overlay closes on navigation");

        // ⏎ on any non-resumable row — this deck's own included — opens a
        // read-only replay instead (the `replay:<id>` lane).
        for (sel, id) in [(0, "ses-mine"), (2, "ses-foreign")] {
            ui.sessions_open = true;
            ui.sessions_sel = sel;
            assert_eq!(
                handle_deck_key(key(KeyCode::Enter), &model, &mut ui),
                DeckAction::Send(WorkspaceInput::SessionOpen { id: id.into() })
            );
            assert!(!ui.sessions_open, "the overlay closes on open too");
        }
    }

    #[test]
    fn paused_sessions_group_between_needs_input_and_cancelled() {
        use crate::envelope::SessionPhase;
        let mut ui = DeckUi::default();
        ui.sessions = vec![
            session_row("c", SessionPhase::Cancelled, false, true),
            session_row("p", SessionPhase::Paused, false, true),
            session_row("n", SessionPhase::NeedsInput, false, false),
        ];
        let order: Vec<&str> = grouped_session_rows(&ui)
            .iter()
            .map(|s| s.id.as_str())
            .collect();
        assert_eq!(order, ["n", "p", "c"]);
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
    fn slash_mcp_search_jumps_straight_into_registry_search() {
        let model = model_with(&["lead"]);
        let mut ui = ready_ui();
        ui.slash_commands = vec![
            SlashCommand::new("/mcp", "mcp"),
            SlashCommand::new("/mcp-search", "search the MCP registry"),
        ];
        for c in "/mcp-search".chars() {
            handle_deck_key(ch(c), &model, &mut ui);
        }
        let action = handle_deck_key(key(KeyCode::Enter), &model, &mut ui);
        assert_eq!(action, DeckAction::Handled, "/mcp-search is deck-local");
        assert_eq!(ui.tab, DeckTab::Mcp);
        assert_eq!(ui.mcp.mode, crate::views::mcp::McpMode::Search);
    }

    #[test]
    fn slash_on_the_mcp_tab_opens_the_command_menu_not_search() {
        // The old `/`-enters-search trigger collided with the command menu;
        // `/` must now behave on the MCP tab exactly as everywhere else —
        // it starts a slash query in the composer.
        let model = model_with(&["lead"]);
        let mut ui = ready_ui();
        ui.slash_commands = vec![SlashCommand::new("/mcp-search", "search")];
        ui.set_tab(DeckTab::Mcp);
        handle_deck_key(ch('/'), &model, &mut ui);
        assert_eq!(
            ui.mcp.mode,
            crate::views::mcp::McpMode::Browse,
            "`/` no longer enters MCP search"
        );
        assert_eq!(ui.composer.buffer(), "/", "the slash query is typing");
        assert!(
            !slash_matches(&ui).is_empty(),
            "…and the command menu is open over it"
        );
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
                oauth: Some(false),
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
                oauth: None,
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

        // `s` enters search mode; typing builds the query; Enter searches.
        // (`/` no longer does — it belongs to the command menu everywhere.)
        handle_deck_key(ch('s'), &model, &mut ui);
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
            oauth: Some(false),
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
    fn a_pasted_secret_lands_in_the_credential_value_never_the_composer() {
        // THE paste-routing security P1: a bracketed paste while the MCP auth
        // VALUE step is focused used to fall through to the global composer, so
        // a pasted API token landed — in plaintext — in the prompt buffer (and
        // could be sent, or shown in the transcript). It must route to the
        // credential value input instead.
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
            oauth: Some(false),
            calls: 0,
        }];
        handle_deck_key(ch('a'), &model, &mut ui); // enter auth
        for c in "TOKEN".chars() {
            handle_deck_key(ch(c), &model, &mut ui);
        }
        handle_deck_key(key(KeyCode::Enter), &model, &mut ui); // → Value step
        assert_eq!(ui.mcp.auth.step, crate::views::mcp::AuthStep::Value);

        // Paste a multi-line secret blob.
        ui.paste("sk-pasted-token\nextra");

        // It went into the credential value (newlines dropped — a one-line
        // field), and did NOT leak into the global composer.
        assert_eq!(ui.mcp.auth.value, "sk-pasted-tokenextra");
        assert!(
            ui.composer.buffer().is_empty(),
            "a pasted secret must never reach the composer: {:?}",
            ui.composer.buffer()
        );
    }

    #[test]
    fn a_paste_in_skills_search_types_into_the_query_not_the_composer() {
        // The SKILLS tab is keyboard-owning: a paste in its search pane must
        // build the query, exactly like typed characters do, never the composer.
        // (paste() is a direct DeckUi method, so no WorkspaceModel is needed.)
        let mut ui = ready_ui();
        ui.set_tab(DeckTab::Skills);
        ui.skills.focus = SkillsFocus::Search;

        ui.paste("postgres\nmigrations");

        assert_eq!(ui.skills.query, "postgresmigrations");
        assert!(
            ui.composer.buffer().is_empty(),
            "a skills-tab paste must not reach the composer: {:?}",
            ui.composer.buffer()
        );
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
            installs: "1.2K installs".into(),
            installs_rank: 1200,
            url: "https://skills.sh/acme/auth".into(),
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
    fn skills_ctrl_o_on_installed_opens_preview_with_local_body() {
        let model = WorkspaceModel::new();
        let mut ui = skills_ui();
        let mut r = a_row("sql-style", SkillScope::Project, true);
        r.body = "# SQL Style\n\nUse lowercase keywords.".to_string();
        ui.skills.view = SkillsView {
            rows: vec![r],
            status: None,
            busy: false,
        };
        // ctrl+o must NOT toggle chain-of-thought on the SKILLS tab — it opens
        // the preview, with the body on hand (no driver round-trip).
        let a = handle_deck_key(ctrl('o'), &model, &mut ui);
        assert_eq!(a, DeckAction::Handled);
        let preview = ui.skills.preview.as_ref().expect("preview opened");
        assert_eq!(preview.pending, None, "installed body is local");
        assert!(
            preview.body.as_deref().unwrap().contains("SQL Style"),
            "body carried from the row"
        );
    }

    #[test]
    fn skills_ctrl_o_on_search_hit_requests_preview_and_shows_loading() {
        let model = WorkspaceModel::new();
        let mut ui = skills_ui();
        ui.skills.focus = SkillsFocus::Search;
        ui.skills.hits = vec![SkillSearchHit {
            id: "acme/auth@oauth".into(),
            installs: "1.2K installs".into(),
            installs_rank: 1200,
            url: "https://skills.sh/acme/auth/oauth".into(),
        }];
        let a = handle_deck_key(ctrl('o'), &model, &mut ui);
        assert_eq!(
            a,
            DeckAction::Send(WorkspaceInput::Skill(SkillOp::Preview {
                id: "acme/auth@oauth".into()
            }))
        );
        let preview = ui.skills.preview.as_ref().expect("preview opened");
        assert_eq!(preview.pending.as_deref(), Some("acme/auth@oauth"));
        assert_eq!(preview.body, None, "loading until the driver replies");
    }

    #[test]
    fn skills_preview_ingest_fills_matching_id_ignores_stale_and_esc_closes() {
        let mut model = WorkspaceModel::new();
        let mut ui = skills_ui();
        ui.skills.preview = Some(SkillPreview {
            title: "acme/auth@oauth".into(),
            subtitle: String::new(),
            pending: Some("acme/auth@oauth".into()),
            body: None,
            scroll: 0,
        });
        // A reply for a DIFFERENT hit is dropped (stale / re-targeted).
        ingest_inbound(
            &Inbound::SkillPreview {
                id: "other/skill@x".into(),
                body: "wrong".into(),
                status: None,
            },
            &mut model,
            &mut ui,
        );
        assert_eq!(ui.skills.preview.as_ref().unwrap().body, None, "stale drop");
        // The matching reply fills the body and clears the pending marker.
        ingest_inbound(
            &Inbound::SkillPreview {
                id: "acme/auth@oauth".into(),
                body: "# OAuth\n\nbody".into(),
                status: None,
            },
            &mut model,
            &mut ui,
        );
        let preview = ui.skills.preview.as_ref().unwrap();
        assert!(preview.body.as_deref().unwrap().contains("OAuth"));
        assert_eq!(preview.pending, None);
        // Esc closes the overlay.
        let a = handle_deck_key(key(KeyCode::Esc), &model, &mut ui);
        assert_eq!(a, DeckAction::Handled);
        assert!(ui.skills.preview.is_none(), "esc closes the preview");
    }

    #[test]
    fn skills_preview_scroll_keys_move_the_offset() {
        let model = WorkspaceModel::new();
        let mut ui = skills_ui();
        ui.skills.preview = Some(SkillPreview {
            title: "x".into(),
            subtitle: String::new(),
            pending: None,
            body: Some("a\nb\nc".into()),
            scroll: 0,
        });
        handle_deck_key(key(KeyCode::Down), &model, &mut ui);
        assert_eq!(ui.skills.preview.as_ref().unwrap().scroll, 1);
        handle_deck_key(key(KeyCode::Up), &model, &mut ui);
        assert_eq!(ui.skills.preview.as_ref().unwrap().scroll, 0);
        handle_deck_key(key(KeyCode::Up), &model, &mut ui);
        assert_eq!(
            ui.skills.preview.as_ref().unwrap().scroll,
            0,
            "clamped at 0"
        );
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
    fn skills_manage_hotkeys_yield_to_a_nonempty_composer() {
        // THE skills-key P1: the installed-pane manage hotkeys (space/e/p/n)
        // were claimed unconditionally, so typing a prompt on the SKILLS tab was
        // hijacked — 'n' opened the create flow, 'e' the edit overlay, space
        // toggled a skill. They must honor the deck-wide "hotkeys only from an
        // empty composer" contract and, mid-prompt, build the prompt instead.
        let model = WorkspaceModel::new();
        let mut ui = skills_ui();
        let r = a_row("sql-style", SkillScope::Project, true);
        ui.skills.view = SkillsView {
            rows: vec![r],
            status: None,
            busy: false,
        };

        // 'r' is not a hotkey → it falls through to the composer, so the
        // composer is now non-empty.
        handle_deck_key(ch('r'), &model, &mut ui);
        assert_eq!(ui.composer.buffer(), "r");

        // Now the manage-hotkey characters type into the composer, not fire.
        for c in "enp e".chars() {
            handle_deck_key(ch(c), &model, &mut ui);
        }
        assert!(
            ui.skills.prompt.is_none(),
            "no manage overlay may open while a prompt is being typed"
        );
        assert!(
            ui.skills.view.rows[0].enabled,
            "space must not toggle the skill mid-prompt"
        );
        assert_eq!(ui.composer.buffer(), "renp e");

        // From an EMPTY composer, 'e' still opens the edit overlay as designed —
        // the gate only defers to a prompt in progress, it doesn't disable the
        // hotkeys.
        ui.composer.clear();
        handle_deck_key(ch('e'), &model, &mut ui);
        assert!(matches!(ui.skills.prompt, Some(SkillPrompt::Edit { .. })));
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

    // ── ISSUES tab ─────────────────────────────────────────────────────────

    use crate::envelope::{EntityField, EntityHit, IssueAction, IssueRow};

    fn issues_ui() -> DeckUi {
        let mut ui = ready_ui();
        ui.set_tab(DeckTab::Issues);
        ui
    }

    fn a_issue(key: &str) -> IssueRow {
        IssueRow {
            key: key.to_string(),
            title: format!("title of {key}"),
            state: "open".into(),
            labels: vec!["bug".into()],
            assignee: Some("@octocat".into()),
            url: format!("https://github.com/o/r/issues/{key}"),
            updated_at: None,
        }
    }

    fn a_hit(kind: &str, label: &str, insert: &str) -> EntityHit {
        EntityHit {
            kind: kind.into(),
            label: label.into(),
            description: format!("about {label}"),
            insert: insert.into(),
        }
    }

    /// Open the create form and move focus to `field`.
    fn form_on(ui: &mut DeckUi, model: &WorkspaceModel, field: IssueField) {
        handle_deck_key(ch('n'), model, ui);
        assert_eq!(ui.issues.mode, IssuesMode::Create);
        while ui.issues.form_field != field {
            handle_deck_key(key(KeyCode::Tab), model, ui);
        }
    }

    #[test]
    fn issues_first_tab_visit_queues_a_refresh() {
        let model = WorkspaceModel::new();
        let mut ui = ready_ui();
        ui.set_tab(DeckTab::Mcp); // ISSUES is Mcp's Tab successor
        handle_deck_key(key(KeyCode::Tab), &model, &mut ui);
        assert_eq!(ui.tab, DeckTab::Issues);
        assert!(ui.issues.busy, "the first visit loads without a keypress");
        assert!(matches!(
            ui.pending_inputs.as_slice(),
            [WorkspaceInput::IssuesRefresh {
                query: None,
                state: None,
                seq: 1,
            }]
        ));
        // A second visit does not re-fetch (busy / loaded gate).
        handle_deck_key(key(KeyCode::Tab), &model, &mut ui);
        handle_deck_key(key(KeyCode::BackTab), &model, &mut ui);
        assert_eq!(ui.pending_inputs.len(), 1, "no duplicate refresh");
    }

    #[test]
    fn issues_browse_keys_refresh_and_start_work() {
        let model = WorkspaceModel::new();
        let mut ui = issues_ui();
        ui.issues.rows = vec![a_issue("#7")];
        ui.issues.loaded = true;
        let action = handle_deck_key(ch('r'), &model, &mut ui);
        assert!(matches!(
            action,
            DeckAction::Send(WorkspaceInput::IssuesRefresh { query: None, .. })
        ));
        let action = handle_deck_key(ch('w'), &model, &mut ui);
        match action {
            DeckAction::Send(WorkspaceInput::IssueAct { key, action, .. }) => {
                assert_eq!(key, "#7");
                assert_eq!(action, IssueAction::StartWork);
            }
            other => panic!("expected IssueAct, got {other:?}"),
        }
    }

    #[test]
    fn issues_tracker_search_fires_on_enter_and_esc_returns() {
        let model = WorkspaceModel::new();
        let mut ui = issues_ui();
        handle_deck_key(ch('/'), &model, &mut ui);
        assert_eq!(ui.issues.mode, IssuesMode::SearchTracker);
        for c in "flaky".chars() {
            handle_deck_key(ch(c), &model, &mut ui);
        }
        assert!(
            ui.composer.buffer().is_empty(),
            "search typing never reaches the composer"
        );
        let action = handle_deck_key(key(KeyCode::Enter), &model, &mut ui);
        match action {
            DeckAction::Send(WorkspaceInput::IssuesRefresh { query, .. }) => {
                assert_eq!(query.as_deref(), Some("flaky"));
            }
            other => panic!("expected IssuesRefresh, got {other:?}"),
        }
        assert_eq!(ui.issues.mode, IssuesMode::Browse);
    }

    #[test]
    fn issues_comment_prompt_sends_the_act_for_the_selected_issue() {
        let model = WorkspaceModel::new();
        let mut ui = issues_ui();
        ui.issues.rows = vec![a_issue("ENG-42")];
        handle_deck_key(ch('c'), &model, &mut ui);
        assert_eq!(ui.issues.mode, IssuesMode::Comment);
        for c in "lgtm".chars() {
            handle_deck_key(ch(c), &model, &mut ui);
        }
        let action = handle_deck_key(key(KeyCode::Enter), &model, &mut ui);
        assert_eq!(
            action,
            DeckAction::Send(WorkspaceInput::IssueAct {
                key: "ENG-42".into(),
                action: IssueAction::Comment("lgtm".into()),
                seq: 1,
            })
        );
    }

    #[test]
    fn typeahead_opens_on_the_first_char_and_fires_per_keystroke() {
        let model = WorkspaceModel::new();
        let mut ui = issues_ui();
        form_on(&mut ui, &model, IssueField::Assignee);
        assert!(!ui.issues.typeahead.open(), "closed until the first char");

        // `@` alone opens the popup and searches the EMPTY query (the
        // backend lists all members for it).
        handle_deck_key(ch('@'), &model, &mut ui);
        assert!(ui.issues.typeahead.open(), "first char opens the popup");
        assert!(matches!(
            ui.pending_inputs.last(),
            Some(WorkspaceInput::EntitySearch {
                field: EntityField::Assignee,
                query,
                ..
            }) if query.is_empty()
        ));

        // Every subsequent edit re-fires — insert and backspace alike.
        handle_deck_key(ch('m'), &model, &mut ui);
        assert!(matches!(
            ui.pending_inputs.last(),
            Some(WorkspaceInput::EntitySearch { query, .. }) if query == "m"
        ));
        handle_deck_key(key(KeyCode::Backspace), &model, &mut ui);
        assert!(matches!(
            ui.pending_inputs.last(),
            Some(WorkspaceInput::EntitySearch { query, .. }) if query.is_empty()
        ));
        assert_eq!(ui.pending_inputs.len(), 3, "one request per keystroke");

        // Deleting the last character closes the popup entirely.
        handle_deck_key(key(KeyCode::Backspace), &model, &mut ui);
        assert!(!ui.issues.typeahead.open(), "empty field ⇒ popup closed");
    }

    #[test]
    fn typeahead_drops_stale_hits_and_applies_the_newest() {
        let model = WorkspaceModel::new();
        let mut ui = issues_ui();
        form_on(&mut ui, &model, IssueField::Assignee);
        handle_deck_key(ch('m'), &model, &mut ui); // seq 1
        handle_deck_key(ch('a'), &model, &mut ui); // seq 2
        let newest_seq = ui.issues.typeahead.seq;
        let mut m = WorkspaceModel::new();

        // The keystroke-1 reply lands late: stale, dropped.
        ingest_inbound(
            &Inbound::EntityHits {
                field: EntityField::Assignee,
                seq: newest_seq - 1,
                query: "m".into(),
                hits: vec![a_hit("Person", "stale", "@stale")],
            },
            &mut m,
            &mut ui,
        );
        assert!(ui.issues.typeahead.hits.is_empty(), "stale reply dropped");

        // The newest reply applies.
        ingest_inbound(
            &Inbound::EntityHits {
                field: EntityField::Assignee,
                seq: newest_seq,
                query: "ma".into(),
                hits: vec![a_hit("Person", "macanderson", "@macanderson")],
            },
            &mut m,
            &mut ui,
        );
        assert_eq!(ui.issues.typeahead.hits.len(), 1);
        assert!(!ui.issues.typeahead.loading);

        // A reply for the WRONG field never lands either.
        ingest_inbound(
            &Inbound::EntityHits {
                field: EntityField::Label,
                seq: newest_seq + 5,
                query: "ma".into(),
                hits: vec![a_hit("Label", "major", "major")],
            },
            &mut m,
            &mut ui,
        );
        assert_eq!(ui.issues.typeahead.hits[0].label, "macanderson");
    }

    #[test]
    fn typeahead_enter_replaces_the_assignee_and_esc_keeps_typed_text() {
        let model = WorkspaceModel::new();
        let mut ui = issues_ui();
        form_on(&mut ui, &model, IssueField::Assignee);
        for c in "mac".chars() {
            handle_deck_key(ch(c), &model, &mut ui);
        }
        ui.issues.typeahead.hits = vec![a_hit("Person", "macanderson", "@macanderson")];
        handle_deck_key(key(KeyCode::Enter), &model, &mut ui);
        assert_eq!(
            ui.issues.form_assignee, "@macanderson",
            "enter REPLACES the assignee field with the hit's insert"
        );
        assert!(!ui.issues.typeahead.open(), "picking closes the popup");
        assert_eq!(ui.issues.mode, IssuesMode::Create, "still in the form");

        // Esc with the popup open closes it but keeps the field text.
        handle_deck_key(ch('x'), &model, &mut ui);
        assert!(ui.issues.typeahead.open());
        handle_deck_key(key(KeyCode::Esc), &model, &mut ui);
        assert!(!ui.issues.typeahead.open());
        assert_eq!(ui.issues.form_assignee, "@macandersonx", "text kept");
        assert_eq!(ui.issues.mode, IssuesMode::Create, "form still open");
    }

    #[test]
    fn typeahead_tab_appends_labels_comma_separated() {
        let model = WorkspaceModel::new();
        let mut ui = issues_ui();
        form_on(&mut ui, &model, IssueField::Labels);
        for c in "bug, urg".chars() {
            handle_deck_key(ch(c), &model, &mut ui);
        }
        // The query is the segment being typed, not the whole field.
        assert!(matches!(
            ui.pending_inputs.last(),
            Some(WorkspaceInput::EntitySearch {
                field: EntityField::Label,
                query,
                ..
            }) if query == "urg"
        ));
        ui.issues.typeahead.hits = vec![a_hit("Label", "urgent", "urgent")];
        handle_deck_key(key(KeyCode::Tab), &model, &mut ui);
        assert_eq!(
            ui.issues.form_labels, "bug, urgent",
            "the picked label replaces the partial segment, comma-appended"
        );
    }

    #[test]
    fn entity_query_and_insert_helpers_cover_both_fields() {
        assert_eq!(entity_query(EntityField::Assignee, "@mac"), "mac");
        assert_eq!(entity_query(EntityField::Assignee, "@"), "");
        assert_eq!(entity_query(EntityField::Label, "bug, ur"), "ur");
        assert_eq!(entity_query(EntityField::Label, "bug"), "bug");

        let mut assignee = "mac".to_string();
        apply_entity_insert(&mut assignee, EntityField::Assignee, "@macanderson");
        assert_eq!(assignee, "@macanderson");

        let mut labels = "ur".to_string();
        apply_entity_insert(&mut labels, EntityField::Label, "urgent");
        assert_eq!(labels, "urgent", "a lone partial segment is replaced");
        labels.push_str(", b");
        apply_entity_insert(&mut labels, EntityField::Label, "bug");
        assert_eq!(labels, "urgent, bug");
    }

    #[test]
    fn issue_form_ctrl_s_submits_the_parsed_fields() {
        let model = WorkspaceModel::new();
        let mut ui = issues_ui();
        form_on(&mut ui, &model, IssueField::Title);
        for c in "Fix the flake".chars() {
            handle_deck_key(ch(c), &model, &mut ui);
        }
        handle_deck_key(key(KeyCode::Enter), &model, &mut ui); // → Body
        assert_eq!(ui.issues.form_field, IssueField::Body);
        for c in "line one".chars() {
            handle_deck_key(ch(c), &model, &mut ui);
        }
        handle_deck_key(key(KeyCode::Enter), &model, &mut ui); // newline in body
        for c in "line two".chars() {
            handle_deck_key(ch(c), &model, &mut ui);
        }
        handle_deck_key(key(KeyCode::Tab), &model, &mut ui); // → Labels
        for c in "bug".chars() {
            handle_deck_key(ch(c), &model, &mut ui);
        }
        // Close the popup the typing opened, then Tab to Assignee.
        handle_deck_key(key(KeyCode::Esc), &model, &mut ui);
        handle_deck_key(key(KeyCode::Tab), &model, &mut ui);
        assert_eq!(ui.issues.form_field, IssueField::Assignee);

        let action = handle_deck_key(ctrl('s'), &model, &mut ui);
        assert_eq!(
            action,
            DeckAction::Send(WorkspaceInput::IssueCreate {
                title: "Fix the flake".into(),
                body: "line one\nline two".into(),
                labels: vec!["bug".into()],
                assignee: None,
                seq: 4, // the three label keystrokes consumed seqs 1–3
            }),
        );
        assert_eq!(ui.issues.mode, IssuesMode::Browse);
        assert!(ui.issues.busy);
    }

    #[test]
    fn issues_list_ingest_folds_rows_clears_busy_and_drops_stale() {
        let mut model = WorkspaceModel::new();
        let mut ui = issues_ui();
        ui.issues.busy = true;
        ui.issues.list_wait = 3;
        ui.issues.sel = 9;

        // A stale reply (an older request) is ignored outright.
        ingest_inbound(
            &Inbound::IssuesList {
                seq: 2,
                outcome: Ok(vec![a_issue("#1")]),
            },
            &mut model,
            &mut ui,
        );
        assert!(ui.issues.rows.is_empty(), "stale list dropped");
        assert!(ui.issues.busy, "…and the newer request is still awaited");

        // The awaited reply folds in: rows, clamped selection, notice.
        ingest_inbound(
            &Inbound::IssuesList {
                seq: 3,
                outcome: Ok(vec![a_issue("#1"), a_issue("#2")]),
            },
            &mut model,
            &mut ui,
        );
        assert_eq!(ui.issues.rows.len(), 2);
        assert_eq!(ui.issues.sel, 1, "selection clamped to the new list");
        assert!(!ui.issues.busy);
        assert!(ui.issues.loaded);
        assert_eq!(
            model.agents.len(),
            0,
            "the model fold ignores the out-of-band list"
        );

        // An error outcome lands in the notice line (the no-tracker hint).
        ui.issues.list_wait = 4;
        ingest_inbound(
            &Inbound::IssuesList {
                seq: 4,
                outcome: Err("no tracker connected — run `stella connect github`".into()),
            },
            &mut model,
            &mut ui,
        );
        assert!(
            ui.issues
                .notice
                .as_deref()
                .is_some_and(|n| n.contains("no tracker connected")),
            "{:?}",
            ui.issues.notice
        );
    }

    #[test]
    fn issue_act_done_ingest_reports_the_outcome() {
        let mut model = WorkspaceModel::new();
        let mut ui = issues_ui();
        ui.issues.act_wait = 2;
        ui.issues.busy = true;
        ingest_inbound(
            &Inbound::IssueActDone {
                seq: 2,
                key: "#7".into(),
                outcome: Ok("created #7 — https://github.com/o/r/issues/7".into()),
            },
            &mut model,
            &mut ui,
        );
        assert!(!ui.issues.busy);
        assert!(
            ui.issues
                .notice
                .as_deref()
                .is_some_and(|n| n.contains("created #7")),
            "{:?}",
            ui.issues.notice
        );
    }

    // ── Help overlay ───────────────────────────────────────────────────────

    #[test]
    fn question_mark_opens_help_overlay() {
        let model = WorkspaceModel::new();
        let mut ui = ready_ui();
        assert!(!ui.help_open);
        handle_deck_key(ch('?'), &model, &mut ui);
        assert!(ui.help_open, "? opens the help overlay");
    }

    #[test]
    fn show_help_inbound_opens_the_overlay_at_the_top() {
        let mut model = WorkspaceModel::new();
        let mut ui = ready_ui();
        // Simulate a prior scroll so we can prove ShowHelp resets it.
        ui.help_scroll.top = 42;
        ui.help_scroll.follow = false;
        ingest_inbound(&Inbound::ShowHelp, &mut model, &mut ui);
        assert!(ui.help_open, "ShowHelp opens the overlay");
        assert_eq!(ui.help_scroll.top, 0, "ShowHelp resets scroll to the top");
        assert!(!ui.help_scroll.follow);
    }

    #[test]
    fn help_overlay_scrolls_with_arrow_keys() {
        let model = WorkspaceModel::new();
        let mut ui = ready_ui();
        ui.help_open = true;
        ui.help_scroll.follow = false;
        // Fake a tall document so scrolling is meaningful.
        ui.metrics.help_total = 100;
        ui.metrics.help_height = 10;
        handle_deck_key(key(KeyCode::Down), &model, &mut ui);
        assert_eq!(ui.help_scroll.top, 1, "↓ scrolls down one line");
        handle_deck_key(key(KeyCode::PageDown), &model, &mut ui);
        assert!(ui.help_scroll.top > 1, "PageDown scrolls by a page");
    }

    #[test]
    fn help_overlay_closes_with_esc_or_q() {
        let model = WorkspaceModel::new();
        let mut ui = ready_ui();
        ui.help_open = true;
        handle_deck_key(key(KeyCode::Esc), &model, &mut ui);
        assert!(!ui.help_open, "Esc closes the help overlay");
        // Re-open and close with q.
        ui.help_open = true;
        handle_deck_key(ch('q'), &model, &mut ui);
        assert!(!ui.help_open, "q closes the help overlay");
    }

    #[test]
    fn help_overlay_does_not_close_on_random_key() {
        let model = WorkspaceModel::new();
        let mut ui = ready_ui();
        ui.help_open = true;
        // A letter that isn't q or ? must NOT close the overlay (it used to —
        // "any key closes" made the long content unreadable).
        handle_deck_key(ch('x'), &model, &mut ui);
        assert!(ui.help_open, "a random key does not close the overlay");
    }
}
