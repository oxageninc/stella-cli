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

use crate::composer::{Composer, SlashCommand};
use crate::input::{ScopeDecision, UserInput};
use crate::model::{AskUserPrompt, SessionModel};
use crate::scroll::ScrollState;

/// Which surface currently has keyboard focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PanelFocus {
    /// The composer: printable keys type, arrows scroll the transcript,
    /// Enter submits. The resting focus of a REPL.
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
    /// Viewport sizes from the last render (for scroll clamping).
    pub metrics: ViewportMetrics,
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
            metrics: ViewportMetrics::default(),
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

    // A pending, unanswered scope card is modal-ish: a/t/x/Esc decide it.
    if model.pending_scope_review.is_some()
        && !ui.scope_answered
        && let Some(decision) = scope_decision_for(key.code)
    {
        ui.scope_answered = true;
        return ShellAction::Submit(UserInput::ScopeDecision(decision));
    }

    // A pending, unanswered `ask_user` question: a number key quick-picks an
    // option (only when nothing is typed), and Enter submits whatever free
    // text has been typed — the always-available affordance the AskUser
    // renderer contract mandates. Anything else falls through to normal
    // composer editing so the user can compose that free-text answer.
    if let Some(prompt) = &model.pending_ask_user
        && !ui.ask_answered
        && let Some(action) = handle_ask_user_key(key.code, prompt, ui)
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
    code: KeyCode,
    prompt: &AskUserPrompt,
    ui: &mut UiState,
) -> Option<ShellAction> {
    match code {
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
        KeyCode::Enter => match ui.composer.take_submission() {
            Some(answer) => {
                ui.ask_answered = true;
                Some(ShellAction::Submit(UserInput::AskUserAnswer {
                    id: prompt.id.clone(),
                    answer,
                }))
            }
            // Empty Enter while a question is pending: force an explicit choice
            // rather than submitting a blank answer.
            None => Some(ShellAction::Ignored),
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
    match key.code {
        KeyCode::Enter => match ui.composer.take_submission() {
            Some(text) => ShellAction::Submit(UserInput::Prompt { text }),
            None => ShellAction::Ignored,
        },
        KeyCode::Backspace => {
            ui.composer.backspace();
            ShellAction::Handled
        }
        KeyCode::Tab => {
            ui.focus = PanelFocus::Files;
            ShellAction::Handled
        }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            ui.composer.insert_char(c);
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
    fn typing_builds_a_prompt_and_enter_submits_it() {
        let model = SessionModel::new();
        let mut ui = UiState::default();
        for c in "hello".chars() {
            assert_eq!(handle_key(ch(c), &model, &mut ui), ShellAction::Handled);
        }
        let action = handle_key(key(KeyCode::Enter), &model, &mut ui);
        assert_eq!(
            action,
            ShellAction::Submit(UserInput::Prompt {
                text: "hello".into()
            })
        );
    }

    #[test]
    fn enter_on_an_empty_composer_is_ignored() {
        let model = SessionModel::new();
        let mut ui = UiState::default();
        assert_eq!(
            handle_key(key(KeyCode::Enter), &model, &mut ui),
            ShellAction::Ignored
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
