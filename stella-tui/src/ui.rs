//! Ephemeral interaction state and the pure key→action mapping.
//!
//! [`UiState`] holds everything that is *not* derived from the event log:
//! scroll anchors, the composer buffer, panel focus, the selected file, and
//! the shell-local "already answered this scope card" guard. Because none of
//! it is reconstructible from — nor should it be — the event stream, it lives
//! here and never in [`crate::model::SessionModel`] (the L-T1 boundary).
//!
//! [`handle_key`] is a **pure function** of `(key, model, &mut ui)` returning
//! a [`ShellAction`]. All of the REPL's decision logic lives here, unit-tested
//! against synthetic `KeyEvent`s, so [`crate::shell`] can be a nearly
//! logic-free event loop (it just forwards actions to the channels and
//! redraws).

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use stella_protocol::AgentEvent;

use crate::composer::{
    Composer, EnterAction, SlashCommand, SlashPopupOutcome, classify_enter, handle_edit_key,
    handle_slash_popup_key, slash_popup_matches,
};
use crate::input::{ScopeDecision, UserInput};
use crate::model::{AskUserPrompt, SessionModel, TranscriptEntry};
use crate::scroll::ScrollState;
use ratatui::text::Line;

/// Which surface currently has keyboard focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PanelFocus {
    /// The composer: printable keys type into a multi-line textarea (`⏎` is
    /// a line break, `⌘⏎`/`⌃⏎` submits), arrows scroll the transcript until
    /// the buffer has content to move through. The resting focus of a REPL.
    #[default]
    Composer,
    /// The files-touched panel: arrows select a file / scroll its diff, Enter
    /// toggles the diff viewer, `q` quits like a pager.
    Files,
}

/// Viewport sizes recorded by the last [`crate::render::render`] pass, so the
/// pure key handler can do line-exact scroll clamping without knowing the
/// terminal size itself. Zero until the first frame is drawn (a keypress
/// before any render is a harmless no-op).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ViewportMetrics {
    pub transcript_height: usize,
    pub transcript_total: usize,
    pub diff_height: usize,
    pub diff_total: usize,
}

/// Memoized transcript render. [`crate::render::transcript_lines`] re-wraps
/// every entry each call (O(transcript)); a session redraws far more often than
/// its transcript changes (every scroll, cursor blink, or files-pane update),
/// so caching the parsed lines keyed on what actually feeds them — the
/// transcript's shape, the wrap width, and thinking expansion — skips that work
/// on unchanged frames.
///
/// The key is a cheap fingerprint, valid because the transcript is append-only
/// except for streaming deltas coalescing into the **trailing** `Text`/
/// `Reasoning` entry (see [`crate::model::SessionModel`]): `len` catches every
/// append and `trailing_stream_len` catches the growing tail. No earlier entry
/// is ever mutated, so no earlier change can slip past this pair.
#[derive(Debug, Clone)]
struct TranscriptCache {
    len: usize,
    trailing_stream_len: usize,
    /// Length of the `TextDelta` streaming preview at fold time — its own
    /// term (see `ensure_transcript_lines`) so clearing the preview always
    /// changes the fingerprint.
    streaming_len: usize,
    expand_thinking: bool,
    width: usize,
    lines: Vec<Line<'static>>,
}

/// All ephemeral view state for one session (see module docs).
#[derive(Debug, Clone)]
pub struct UiState {
    /// Transcript scrollback anchor.
    pub scroll: ScrollState,
    /// Diff-viewer scrollback anchor.
    pub diff_scroll: ScrollState,
    /// The input line.
    pub composer: Composer,
    /// Which surface has focus.
    pub focus: PanelFocus,
    /// Index into [`SessionModel::files`](crate::model::SessionModel::files)
    /// of the selected file.
    pub selected_file: usize,
    /// Whether the diff viewer is open for the selected file.
    pub diff_open: bool,
    /// Shell-local guard: set when the user answers the current scope card so
    /// the actionable card flips to "awaiting engine…" and a second keypress
    /// cannot double-submit. Reset by [`ingest`] on a fresh `ScopeReview`.
    pub scope_answered: bool,
    /// The same guard for a pending `ask_user` question. Reset by [`ingest`]
    /// on a fresh `AskUser`.
    pub ask_answered: bool,
    /// The slash-command vocabulary offered by the menu (an input — the CLI
    /// owns the real list).
    pub slash_commands: Vec<SlashCommand>,
    /// Selected row in the slash popup while it is open (clamped to the
    /// filtered matches at use time).
    pub slash_selected: usize,
    /// Whether reasoning entries render in full. Off by default — collapsed
    /// thinking shows a one-line live tail; `ctrl+r` toggles.
    pub thinking_expanded: bool,
    /// Whether the terminal is a *legacy* one (no kitty keyboard protocol).
    /// Enter semantics are universal now — bare `⏎` submits, a modified `⏎`
    /// breaks (see [`classify_enter`]) — so this only selects which newline
    /// chord the hint advertises (`⌥⏎` on legacy, `⌘⏎` where reportable). The
    /// shell sets it from the terminal's actual capability.
    pub enter_submits: bool,
    /// Viewport sizes from the last render (for scroll clamping).
    pub metrics: ViewportMetrics,
    /// Memoized transcript render (see [`TranscriptCache`]). Private: the render
    /// path reads it only through [`UiState::transcript_lines`].
    transcript_cache: Option<TranscriptCache>,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            scroll: ScrollState::default(),
            diff_scroll: ScrollState::default(),
            composer: Composer::new(),
            focus: PanelFocus::default(),
            selected_file: 0,
            diff_open: false,
            scope_answered: false,
            ask_answered: false,
            slash_commands: Vec::new(),
            slash_selected: 0,
            thinking_expanded: false,
            enter_submits: false,
            metrics: ViewportMetrics::default(),
            transcript_cache: None,
        }
    }
}

impl UiState {
    /// A UI state with an explicit composer and slash-command vocabulary.
    pub fn new(composer: Composer, slash_commands: Vec<SlashCommand>) -> Self {
        Self {
            composer,
            slash_commands,
            ..Self::default()
        }
    }

    /// Rebuild the memoized transcript render only when the transcript shape,
    /// wrap width, or thinking expansion changed. Call once per frame before
    /// [`transcript_lines`](Self::transcript_lines); on an unchanged frame this
    /// is an O(1) fingerprint check that skips the whole re-wrap.
    pub fn ensure_transcript_lines(
        &mut self,
        model: &SessionModel,
        expand_thinking: bool,
        width: usize,
    ) {
        let len = model.transcript.len();
        let trailing_stream_len = match model.transcript.last() {
            Some(TranscriptEntry::Text(s) | TranscriptEntry::Reasoning(s)) => s.len(),
            _ => 0,
        };
        // A separate fingerprint term — not summed into the trailing one, or
        // the authoritative `Text` coalescing a cleared preview into the
        // trailing entry could leave the total unchanged and the cache stale.
        let streaming_len = model.streaming_text.len();
        let fresh = self.transcript_cache.as_ref().is_some_and(|c| {
            c.len == len
                && c.trailing_stream_len == trailing_stream_len
                && c.streaming_len == streaming_len
                && c.expand_thinking == expand_thinking
                && c.width == width
        });
        if !fresh {
            let lines = crate::render::transcript_lines(model, expand_thinking, width);
            self.transcript_cache = Some(TranscriptCache {
                len,
                trailing_stream_len,
                streaming_len,
                expand_thinking,
                width,
                lines,
            });
        }
    }

    /// The transcript lines from the most recent
    /// [`ensure_transcript_lines`](Self::ensure_transcript_lines) — empty until
    /// it has run at least once.
    pub fn transcript_lines(&self) -> &[Line<'static>] {
        self.transcript_cache.as_ref().map_or(&[], |c| &c.lines)
    }
}

/// The outcome of handling one key — the shell's entire vocabulary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellAction {
    /// The key was not meaningful; no redraw needed.
    Ignored,
    /// State changed; redraw.
    Handled,
    /// Forward this to the engine (and redraw).
    Submit(UserInput),
    /// Tear down and exit (the shell also sends [`UserInput::Cancel`] first).
    Quit,
}

/// Apply one incoming event to both the derived model and the ephemeral UI,
/// keeping the two consistent. The model fold is the sole state mutation
/// (L-T1); the only UI reaction is resetting the scope-answer guard when a
/// *new* scope card appears, and clamping the selected-file index as files
/// come and go. Pure and unit-tested so the shell need not carry this logic.
pub fn ingest(event: &AgentEvent, model: &mut SessionModel, ui: &mut UiState) {
    if matches!(event, AgentEvent::ScopeReview { .. }) {
        ui.scope_answered = false;
    }
    if matches!(event, AgentEvent::AskUser { .. }) {
        ui.ask_answered = false;
    }
    model.apply(event);
    // Keep the file selection in range as the touched-files set grows.
    if !model.files.is_empty() {
        ui.selected_file = ui.selected_file.min(model.files.len() - 1);
    } else {
        ui.selected_file = 0;
    }
}

/// Map one key to a [`ShellAction`], mutating `ui` in place. Pure over
/// `(key, model)`; all REPL behavior is decided here.
pub fn handle_key(key: KeyEvent, model: &SessionModel, ui: &mut UiState) -> ShellAction {
    // Only react to presses/repeats — some terminals also deliver Release.
    if key.kind == KeyEventKind::Release {
        return ShellAction::Ignored;
    }

    // Ctrl-C always requests a clean cancel + quit, from any focus.
    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
        return ShellAction::Quit;
    }

    // Ctrl-R toggles the collapsed-thinking view from any focus.
    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('r')) {
        ui.thinking_expanded = !ui.thinking_expanded;
        return ShellAction::Handled;
    }

    // A pending, unanswered scope card is modal-ish: a/t/x/Esc decide it — but
    // only from an empty composer. The decision keys are ordinary prompt
    // characters, so once the user starts typing a message (e.g. "add a table")
    // the keys must build the prompt, not silently approve/trim/abort the gate.
    // Mirrors the deck (`crate::deck_ui`) and the ask_user quick-pick's
    // "only when nothing is typed" rule.
    if model.pending_scope_review.is_some()
        && !ui.scope_answered
        && ui.composer.buffer().is_empty()
        && let Some(decision) = scope_decision_for(key.code)
    {
        ui.scope_answered = true;
        return ShellAction::Submit(UserInput::ScopeDecision(decision));
    }

    // A pending, unanswered `ask_user` question: a number key quick-picks an
    // option (only when nothing is typed), and the submit chord dispatches
    // whatever free text has been typed — the always-available affordance
    // the AskUser renderer contract mandates. Anything else (including a
    // plain `⏎` line break) falls through to normal composer editing so the
    // user can compose a multi-line free-text answer.
    if let Some(prompt) = &model.pending_ask_user
        && !ui.ask_answered
        && let Some(action) = handle_ask_user_key(key, prompt, ui)
    {
        return action;
    }

    match ui.focus {
        PanelFocus::Composer => handle_composer_key(key, ui),
        PanelFocus::Files => handle_files_key(key, model, ui),
    }
}

/// The `ask_user` card key bindings. Returns `Some(action)` to short-circuit
/// (a quick-pick or a free-text submit) or `None` to fall through to normal
/// composer editing so the user can keep typing a free-text answer.
fn handle_ask_user_key(
    key: KeyEvent,
    prompt: &AskUserPrompt,
    ui: &mut UiState,
) -> Option<ShellAction> {
    match key.code {
        // A digit quick-picks an option — but only when nothing has been
        // typed, so a free-text answer beginning with a digit is unaffected.
        KeyCode::Char(d @ '1'..='9') if ui.composer.buffer().is_empty() => {
            let idx = (d as usize) - ('1' as usize);
            match prompt.options.get(idx) {
                Some(option) => {
                    ui.ask_answered = true;
                    Some(ShellAction::Submit(UserInput::AskUserAnswer {
                        id: prompt.id.clone(),
                        answer: option.clone(),
                    }))
                }
                // Out-of-range digit: let it type into the free-text answer.
                None => None,
            }
        }
        KeyCode::Enter => match classify_enter(&key) {
            EnterAction::Submit => match ui.composer.take_submission() {
                Some(submission) => {
                    ui.ask_answered = true;
                    Some(ShellAction::Submit(UserInput::AskUserAnswer {
                        id: prompt.id.clone(),
                        answer: submission.text,
                    }))
                }
                // An empty submit while a question is pending: force an
                // explicit choice rather than submitting a blank answer.
                None => Some(ShellAction::Ignored),
            },
            // A line break: fall through to normal composer editing so the
            // free-text answer can span lines.
            _ => None,
        },
        _ => None,
    }
}

/// The scope-card key bindings, or `None` for a key that isn't a decision.
fn scope_decision_for(code: KeyCode) -> Option<ScopeDecision> {
    match code {
        KeyCode::Char('a') => Some(ScopeDecision::Approve),
        KeyCode::Char('t') => Some(ScopeDecision::Trim),
        KeyCode::Char('x') | KeyCode::Esc => Some(ScopeDecision::Abort),
        _ => None,
    }
}

fn handle_composer_key(key: KeyEvent, ui: &mut UiState) -> ShellAction {
    // While the slash popup is open, navigation keys drive it: ↑/↓ choose,
    // Tab completes into the buffer, Enter runs the selection, Esc dismisses.
    // Shared with the deck (`crate::deck_ui`) via `crate::composer` so both
    // surfaces stay consistent by construction.
    let slash = slash_popup_matches(&ui.composer, &ui.slash_commands);
    if !slash.is_empty()
        && let Some(outcome) =
            handle_slash_popup_key(key, &slash, &mut ui.composer, &mut ui.slash_selected)
    {
        return match outcome {
            SlashPopupOutcome::Handled => ShellAction::Handled,
            SlashPopupOutcome::Submit(text) => ShellAction::Submit(UserInput::Prompt {
                text,
                attachments: Vec::new(),
            }),
        };
    }
    // Enter is a textarea key: plain `⏎` breaks the line (preserved verbatim
    // in the submitted prompt), the `⌘⏎`/`⌃⏎` chord submits — flipped on
    // legacy terminals that can't report a modified Enter (`enter_submits`).
    match classify_enter(&key) {
        EnterAction::Submit => {
            return match ui.composer.take_submission() {
                Some(submission) => ShellAction::Submit(UserInput::Prompt {
                    text: submission.text,
                    attachments: submission.attachments,
                }),
                None => ShellAction::Ignored,
            };
        }
        EnterAction::Newline => {
            // No line breaks into a fully blank composer — a stray leading
            // newline is never what an empty ⏎ meant.
            return if ui.composer.is_blank() {
                ShellAction::Ignored
            } else {
                ui.composer.insert_newline();
                ShellAction::Handled
            };
        }
        EnterAction::NotEnter => {}
    }
    // Cursor motion (←/→/↑/↓/Home/End, ⌥[ ⌥] jumps) — gated inside on the
    // buffer having content, so an empty composer still scrolls the
    // transcript with these keys.
    if handle_edit_key(key, &mut ui.composer) {
        return ShellAction::Handled;
    }
    match key.code {
        KeyCode::Backspace => {
            ui.composer.backspace();
            ui.slash_selected = 0;
            ShellAction::Handled
        }
        KeyCode::Tab => {
            ui.focus = PanelFocus::Files;
            ShellAction::Handled
        }
        KeyCode::Char(c)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER | KeyModifiers::META) =>
        {
            ui.composer.insert_char(c);
            ui.slash_selected = 0;
            ShellAction::Handled
        }
        // Non-printable navigation scrolls the transcript (or the diff when
        // it is open over the composer's line-of-sight).
        _ => scroll_nav(key.code, ui, ScrollTarget::TranscriptOrDiff),
    }
}

fn handle_files_key(key: KeyEvent, model: &SessionModel, ui: &mut UiState) -> ShellAction {
    let file_count = model.files.len();
    match key.code {
        // `q` quits from a panel focus, pager-style (typing 'q' is unaffected
        // because that happens in Composer focus).
        KeyCode::Char('q') => ShellAction::Quit,
        KeyCode::Tab => {
            ui.focus = PanelFocus::Composer;
            ShellAction::Handled
        }
        KeyCode::Esc => {
            if ui.diff_open {
                ui.diff_open = false;
                ShellAction::Handled
            } else {
                ui.focus = PanelFocus::Composer;
                ShellAction::Handled
            }
        }
        KeyCode::Enter => {
            if file_count > 0 {
                ui.diff_open = !ui.diff_open;
                ui.diff_scroll = ScrollState::default();
                ShellAction::Handled
            } else {
                ShellAction::Ignored
            }
        }
        _ if ui.diff_open => scroll_nav(key.code, ui, ScrollTarget::Diff),
        // Browsing the file list.
        KeyCode::Up => {
            ui.selected_file = ui.selected_file.saturating_sub(1);
            ShellAction::Handled
        }
        KeyCode::Down => {
            if file_count > 0 {
                ui.selected_file = (ui.selected_file + 1).min(file_count - 1);
            }
            ShellAction::Handled
        }
        KeyCode::Home => {
            ui.selected_file = 0;
            ShellAction::Handled
        }
        KeyCode::End => {
            ui.selected_file = file_count.saturating_sub(1);
            ShellAction::Handled
        }
        _ => ShellAction::Ignored,
    }
}

/// Which scrollable a navigation key drives.
enum ScrollTarget {
    /// The diff viewer when open, else the transcript.
    TranscriptOrDiff,
    /// The diff viewer specifically.
    Diff,
}

fn scroll_nav(code: KeyCode, ui: &mut UiState, target: ScrollTarget) -> ShellAction {
    let use_diff = match target {
        ScrollTarget::Diff => true,
        ScrollTarget::TranscriptOrDiff => ui.diff_open,
    };
    let (state, total, height) = if use_diff {
        (
            &mut ui.diff_scroll,
            ui.metrics.diff_total,
            ui.metrics.diff_height,
        )
    } else {
        (
            &mut ui.scroll,
            ui.metrics.transcript_total,
            ui.metrics.transcript_height,
        )
    };
    match code {
        KeyCode::Up => state.scroll_up(1, total, height),
        KeyCode::Down => state.scroll_down(1, total, height),
        KeyCode::PageUp => state.page_up(total, height),
        KeyCode::PageDown => state.page_down(total, height),
        KeyCode::Home => state.to_top(),
        KeyCode::End => state.to_bottom(),
        _ => return ShellAction::Ignored,
    }
    ShellAction::Handled
}

#[cfg(test)]
// Test fixtures build a default `UiState` and then poke one or two fields to
// set up a scenario; struct-update syntax for each would only obscure intent.
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use stella_protocol::{ScopeProposal, StageKind};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }
    fn ch(c: char) -> KeyEvent {
        key(KeyCode::Char(c))
    }
    /// The newline chord — `⌘⏎` as the kitty keyboard protocol reports it
    /// (a modified Enter inserts a line break; a bare Enter submits).
    fn cmd_enter() -> KeyEvent {
        KeyEvent::new(KeyCode::Enter, KeyModifiers::SUPER)
    }
    fn alt(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT)
    }

    fn model_with_scope() -> SessionModel {
        let mut m = SessionModel::new();
        m.apply(&AgentEvent::ScopeReview {
            proposal: ScopeProposal {
                summary: "x".into(),
                steps: vec![],
                estimated_files: 3,
                estimated_cost_usd: None,
            },
        });
        m
    }

    #[test]
    fn typing_builds_a_prompt_and_bare_enter_sends_it() {
        let model = SessionModel::new();
        let mut ui = UiState::default();
        for c in "hello".chars() {
            assert_eq!(handle_key(ch(c), &model, &mut ui), ShellAction::Handled);
        }
        let action = handle_key(key(KeyCode::Enter), &model, &mut ui);
        assert_eq!(
            action,
            ShellAction::Submit(UserInput::Prompt {
                text: "hello".into(),
                attachments: Vec::new(),
            })
        );
    }

    #[test]
    fn a_modified_enter_inserts_a_line_break_preserved_through_submit() {
        let model = SessionModel::new();
        let mut ui = UiState::default();
        for c in "line one".chars() {
            handle_key(ch(c), &model, &mut ui);
        }
        assert_eq!(
            handle_key(cmd_enter(), &model, &mut ui),
            ShellAction::Handled,
            "⌘⏎ is a line break, not a submit"
        );
        for c in "line two".chars() {
            handle_key(ch(c), &model, &mut ui);
        }
        let action = handle_key(key(KeyCode::Enter), &model, &mut ui);
        assert_eq!(
            action,
            ShellAction::Submit(UserInput::Prompt {
                text: "line one\nline two".into(),
                attachments: Vec::new(),
            }),
            "the typed line break survives into the submitted prompt"
        );
    }

    #[test]
    fn enter_on_an_empty_composer_is_ignored() {
        let model = SessionModel::new();
        let mut ui = UiState::default();
        assert_eq!(
            handle_key(key(KeyCode::Enter), &model, &mut ui),
            ShellAction::Ignored,
            "no stray leading newline into a blank composer"
        );
        assert_eq!(
            handle_key(cmd_enter(), &model, &mut ui),
            ShellAction::Ignored,
            "nothing to submit either"
        );
    }

    #[test]
    fn alt_brackets_jump_the_cursor_to_start_and_end() {
        let model = SessionModel::new();
        let mut ui = UiState::default();
        for c in "abc".chars() {
            handle_key(ch(c), &model, &mut ui);
        }
        assert_eq!(ui.composer.cursor(), 3);
        assert_eq!(handle_key(alt('['), &model, &mut ui), ShellAction::Handled);
        assert_eq!(ui.composer.cursor(), 0, "⌥[ → before the first character");
        assert_eq!(handle_key(alt(']'), &model, &mut ui), ShellAction::Handled);
        assert_eq!(ui.composer.cursor(), 3, "⌥] → one past the last character");
    }

    #[test]
    fn bare_enter_submits_and_a_modified_enter_inserts_a_break() {
        let model = SessionModel::new();
        let mut ui = UiState::default();
        for c in "hi".chars() {
            handle_key(ch(c), &model, &mut ui);
        }
        let alt_enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT);
        assert_eq!(
            handle_key(alt_enter, &model, &mut ui),
            ShellAction::Handled,
            "⌥⏎ inserts a line break"
        );
        assert_eq!(ui.composer.buffer(), "hi\n");
        let action = handle_key(key(KeyCode::Enter), &model, &mut ui);
        assert_eq!(
            action,
            ShellAction::Submit(UserInput::Prompt {
                text: "hi\n".into(),
                attachments: Vec::new(),
            }),
            "bare ⏎ submits (never blocks)"
        );
    }

    #[test]
    fn arrows_edit_a_multiline_prompt_and_typing_lands_at_the_cursor() {
        let model = SessionModel::new();
        let mut ui = UiState::default();
        for c in "ab".chars() {
            handle_key(ch(c), &model, &mut ui);
        }
        handle_key(cmd_enter(), &model, &mut ui); // ⌘⏎ inserts a line break
        for c in "cd".chars() {
            handle_key(ch(c), &model, &mut ui);
        }
        handle_key(key(KeyCode::Up), &model, &mut ui);
        assert_eq!(ui.composer.cursor(), 2, "column kept on the line above");
        handle_key(key(KeyCode::Left), &model, &mut ui);
        handle_key(ch('X'), &model, &mut ui);
        assert_eq!(ui.composer.buffer(), "aXb\ncd", "typed at the cursor");
        assert!(
            ui.scroll.follow,
            "cursor motion never touched the transcript scroll"
        );
    }

    #[test]
    fn ctrl_c_quits_from_any_focus() {
        let model = SessionModel::new();
        let mut ui = UiState::default();
        assert_eq!(handle_key(ctrl('c'), &model, &mut ui), ShellAction::Quit);
        ui.focus = PanelFocus::Files;
        assert_eq!(handle_key(ctrl('c'), &model, &mut ui), ShellAction::Quit);
    }

    #[test]
    fn q_quits_only_from_the_files_panel_and_types_in_the_composer() {
        let model = SessionModel::new();
        let mut ui = UiState::default();
        // In composer focus 'q' is just a character.
        assert_eq!(handle_key(ch('q'), &model, &mut ui), ShellAction::Handled);
        assert_eq!(ui.composer.buffer(), "q");
        // In files focus it quits.
        ui.focus = PanelFocus::Files;
        assert_eq!(handle_key(ch('q'), &model, &mut ui), ShellAction::Quit);
    }

    #[test]
    fn scope_card_keys_submit_a_decision_once() {
        let model = model_with_scope();
        let mut ui = UiState::default();
        let action = handle_key(ch('a'), &model, &mut ui);
        assert_eq!(
            action,
            ShellAction::Submit(UserInput::ScopeDecision(ScopeDecision::Approve))
        );
        assert!(ui.scope_answered);
        // A second key no longer submits a decision — it types instead (the
        // guard prevents a double-answer).
        let action2 = handle_key(ch('a'), &model, &mut ui);
        assert_eq!(action2, ShellAction::Handled);
        assert_eq!(ui.composer.buffer(), "a");
    }

    #[test]
    fn scope_card_esc_and_x_abort() {
        for code in [KeyCode::Char('x'), KeyCode::Esc] {
            let model = model_with_scope();
            let mut ui = UiState::default();
            assert_eq!(
                handle_key(key(code), &model, &mut ui),
                ShellAction::Submit(UserInput::ScopeDecision(ScopeDecision::Abort))
            );
        }
    }

    #[test]
    fn scope_card_keys_yield_to_a_prompt_being_typed() {
        // THE scope-key P1: the decision keys (a/t/x) are ordinary prompt
        // letters and used to fire even mid-prompt, so typing a message like
        // "add a table" while a scope card was up silently approved/aborted the
        // gate. They must defer to a non-empty composer — same rule the deck and
        // the ask_user quick-pick already use.
        let model = model_with_scope();
        let mut ui = UiState::default();

        // A leading non-decision char types and makes the composer non-empty.
        assert_eq!(handle_key(ch('f'), &model, &mut ui), ShellAction::Handled);
        assert!(!ui.scope_answered, "typing must not answer the gate");

        // Now a/t/x build the prompt instead of deciding.
        for c in "atx".chars() {
            assert_eq!(handle_key(ch(c), &model, &mut ui), ShellAction::Handled);
        }
        assert!(
            !ui.scope_answered,
            "no decision submitted while a prompt is typed"
        );
        assert_eq!(ui.composer.buffer(), "fatx");

        // From an empty composer the gate still decides on a single key.
        ui.composer.clear();
        assert_eq!(
            handle_key(ch('a'), &model, &mut ui),
            ShellAction::Submit(UserInput::ScopeDecision(ScopeDecision::Approve))
        );
    }

    #[test]
    fn ingest_resets_the_scope_guard_on_a_fresh_card() {
        let mut model = SessionModel::new();
        let mut ui = UiState::default();
        ui.scope_answered = true; // answered a previous card
        ingest(
            &AgentEvent::ScopeReview {
                proposal: ScopeProposal {
                    summary: "y".into(),
                    steps: vec![],
                    estimated_files: 1,
                    estimated_cost_usd: None,
                },
            },
            &mut model,
            &mut ui,
        );
        assert!(!ui.scope_answered, "a new card re-arms the decision keys");
    }

    fn model_with_ask() -> SessionModel {
        let mut m = SessionModel::new();
        m.apply(&AgentEvent::AskUser {
            id: "call_ask_1".into(),
            question: "which db?".into(),
            options: vec!["postgres".into(), "sqlite".into()],
        });
        m
    }

    #[test]
    fn ask_user_number_key_quick_picks_an_option() {
        let model = model_with_ask();
        let mut ui = UiState::default();
        let action = handle_key(ch('2'), &model, &mut ui);
        assert_eq!(
            action,
            ShellAction::Submit(UserInput::AskUserAnswer {
                id: "call_ask_1".into(),
                answer: "sqlite".into(),
            })
        );
        assert!(ui.ask_answered);
    }

    #[test]
    fn ask_user_free_text_answer_is_always_available() {
        // The renderer contract mandates a free-text affordance on every
        // question — typing then Enter submits it, not a new prompt.
        let model = model_with_ask();
        let mut ui = UiState::default();
        for c in "mysql".chars() {
            handle_key(ch(c), &model, &mut ui);
        }
        let action = handle_key(key(KeyCode::Enter), &model, &mut ui);
        assert_eq!(
            action,
            ShellAction::Submit(UserInput::AskUserAnswer {
                id: "call_ask_1".into(),
                answer: "mysql".into(),
            })
        );
    }

    #[test]
    fn ask_user_free_text_answer_can_span_lines() {
        // A modified ⏎ while a question is pending is a line break in the
        // answer, not a submit — a bare ⏎ dispatches the whole multi-line text.
        let model = model_with_ask();
        let mut ui = UiState::default();
        for c in "two".chars() {
            handle_key(ch(c), &model, &mut ui);
        }
        handle_key(cmd_enter(), &model, &mut ui);
        for c in "lines".chars() {
            handle_key(ch(c), &model, &mut ui);
        }
        let action = handle_key(key(KeyCode::Enter), &model, &mut ui);
        assert_eq!(
            action,
            ShellAction::Submit(UserInput::AskUserAnswer {
                id: "call_ask_1".into(),
                answer: "two\nlines".into(),
            })
        );
    }

    #[test]
    fn ask_user_digit_typed_into_free_text_is_not_a_quick_pick() {
        let model = model_with_ask();
        let mut ui = UiState::default();
        // Start a free-text answer, THEN a digit — it must type, not pick.
        handle_key(ch('p'), &model, &mut ui);
        handle_key(ch('1'), &model, &mut ui);
        assert_eq!(ui.composer.buffer(), "p1");
        assert!(!ui.ask_answered);
    }

    #[test]
    fn ask_user_out_of_range_digit_falls_through_to_typing() {
        let model = model_with_ask(); // only 2 options
        let mut ui = UiState::default();
        let action = handle_key(ch('9'), &model, &mut ui);
        assert_eq!(action, ShellAction::Handled);
        assert_eq!(ui.composer.buffer(), "9");
    }

    #[test]
    fn ingest_resets_the_ask_guard_on_a_fresh_question() {
        let mut model = SessionModel::new();
        let mut ui = UiState::default();
        ui.ask_answered = true;
        ingest(
            &AgentEvent::AskUser {
                id: "q2".into(),
                question: "x".into(),
                options: vec![],
            },
            &mut model,
            &mut ui,
        );
        assert!(!ui.ask_answered);
    }

    #[test]
    fn ctrl_r_toggles_the_thinking_view() {
        let model = SessionModel::new();
        let mut ui = UiState::default();
        assert!(!ui.thinking_expanded, "collapsed by default");
        assert_eq!(handle_key(ctrl('r'), &model, &mut ui), ShellAction::Handled);
        assert!(ui.thinking_expanded);
        handle_key(ctrl('r'), &model, &mut ui);
        assert!(!ui.thinking_expanded);
    }

    fn slash_ui() -> UiState {
        UiState::new(
            Composer::new(),
            vec![
                SlashCommand::new("/help", "show help"),
                SlashCommand::new("/diff", "open the diff viewer"),
                SlashCommand::new("/files", "focus files"),
            ],
        )
    }

    #[test]
    fn slash_popup_arrows_choose_and_enter_runs_the_selection() {
        let model = SessionModel::new();
        let mut ui = slash_ui();
        handle_key(ch('/'), &model, &mut ui);
        handle_key(key(KeyCode::Down), &model, &mut ui);
        assert_eq!(ui.slash_selected, 1);
        let action = handle_key(key(KeyCode::Enter), &model, &mut ui);
        assert_eq!(
            action,
            ShellAction::Submit(UserInput::Prompt {
                text: "/diff".into(),
                attachments: Vec::new(),
            })
        );
        assert!(ui.composer.is_empty(), "running a command clears the line");
    }

    #[test]
    fn slash_popup_tab_completes_without_submitting() {
        let model = SessionModel::new();
        let mut ui = slash_ui();
        for c in "/f".chars() {
            handle_key(ch(c), &model, &mut ui);
        }
        let action = handle_key(key(KeyCode::Tab), &model, &mut ui);
        assert_eq!(action, ShellAction::Handled);
        assert_eq!(ui.composer.buffer(), "/files", "completed in place");
    }

    #[test]
    fn slash_popup_esc_dismisses_and_typing_resets_the_selection() {
        let model = SessionModel::new();
        let mut ui = slash_ui();
        handle_key(ch('/'), &model, &mut ui);
        handle_key(key(KeyCode::Down), &model, &mut ui);
        assert_eq!(ui.slash_selected, 1);
        handle_key(ch('h'), &model, &mut ui);
        assert_eq!(ui.slash_selected, 0, "typing narrows → selection resets");
        handle_key(key(KeyCode::Esc), &model, &mut ui);
        assert!(ui.composer.is_empty(), "esc clears the slash query");
    }

    #[test]
    fn arrows_still_scroll_when_no_slash_popup_is_active() {
        let model = SessionModel::new();
        let mut ui = slash_ui();
        ui.metrics = ViewportMetrics {
            transcript_height: 10,
            transcript_total: 100,
            ..Default::default()
        };
        handle_key(key(KeyCode::Up), &model, &mut ui);
        assert!(!ui.scroll.follow, "no popup → arrows scroll the transcript");
    }

    #[test]
    fn tab_toggles_focus_between_composer_and_files() {
        let model = SessionModel::new();
        let mut ui = UiState::default();
        assert_eq!(ui.focus, PanelFocus::Composer);
        handle_key(key(KeyCode::Tab), &model, &mut ui);
        assert_eq!(ui.focus, PanelFocus::Files);
        handle_key(key(KeyCode::Tab), &model, &mut ui);
        assert_eq!(ui.focus, PanelFocus::Composer);
    }

    #[test]
    fn arrows_scroll_the_transcript_from_composer_focus() {
        let model = SessionModel::new();
        let mut ui = UiState::default();
        ui.metrics = ViewportMetrics {
            transcript_height: 10,
            transcript_total: 100,
            ..Default::default()
        };
        assert!(ui.scroll.follow);
        handle_key(key(KeyCode::Up), &model, &mut ui);
        assert!(!ui.scroll.follow, "scrolling up leaves follow-mode");
        assert_eq!(ui.scroll.window(100, 10), 89..99);
    }

    #[test]
    fn enter_in_files_focus_toggles_the_diff_viewer() {
        let mut model = SessionModel::new();
        model.apply(&AgentEvent::FileChange {
            path: "a.rs".into(),
            kind: stella_protocol::FileChangeKind::Modified,
            diff: Some("@@\n-a\n+b".into()),
        });
        let mut ui = UiState::default();
        ui.focus = PanelFocus::Files;
        assert!(!ui.diff_open);
        handle_key(key(KeyCode::Enter), &model, &mut ui);
        assert!(ui.diff_open);
        handle_key(key(KeyCode::Enter), &model, &mut ui);
        assert!(!ui.diff_open);
    }

    #[test]
    fn file_selection_clamps_as_files_appear() {
        let mut model = SessionModel::new();
        let mut ui = UiState::default();
        ui.selected_file = 9; // stale, out of range
        ingest(
            &AgentEvent::FileChange {
                path: "a.rs".into(),
                kind: stella_protocol::FileChangeKind::Created,
                diff: None,
            },
            &mut model,
            &mut ui,
        );
        assert_eq!(ui.selected_file, 0, "clamped to the only file");
    }

    #[test]
    fn stage_events_flow_through_ingest_into_the_model() {
        let mut model = SessionModel::new();
        let mut ui = UiState::default();
        ingest(
            &AgentEvent::Stage {
                name: StageKind::Plan,
            },
            &mut model,
            &mut ui,
        );
        assert_eq!(model.hud.stage, Some(StageKind::Plan));
    }
}
