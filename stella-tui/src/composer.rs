//! The composer: the input-line model, paste-chip collapse (L-T3), and the
//! slash-command menu.
//!
//! A paste above a small line threshold never floods the input buffer or the
//! model context (the TS bug L-T3 fixed): it collapses to a
//! `[pasted: N lines]` chip in the composer while the **full payload stays
//! attached** to the pending message. On submit, chips expand back to their
//! full text — the model sees everything, the screen sees a chip.
//!
//! The slash menu is deliberately generic: it filters a caller-supplied
//! command list by the typed prefix, so `/help /clear /models /diff /files`
//! are an *input*, not a hard-coded set — the CLI owns the real command
//! vocabulary.
//!
//! [`handle_slash_popup_key`] is the one implementation of slash-popup key
//! handling, shared by every composer-driven surface (the single-session
//! REPL's [`crate::ui`] and the deck's [`crate::deck_ui`]) so a future fix to
//! selection clamping, Esc semantics, or completion behavior can't land on
//! one surface and drift from the other.

use crossterm::event::{KeyCode, KeyEvent};

/// Below this many lines a paste is inserted inline; at or above it, the
/// paste collapses to a chip. Small on purpose (L-T3).
pub const DEFAULT_PASTE_LINE_THRESHOLD: usize = 6;

/// One piece of composer content: either typed text or a collapsed paste.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposerEntry {
    /// Literal typed text.
    Text(String),
    /// A collapsed paste. `full_text` is what the model receives; the display
    /// shows `[pasted: line_count lines]`.
    Chip {
        full_text: String,
        line_count: usize,
    },
}

impl ComposerEntry {
    /// The on-screen representation — the full payload never renders raw
    /// (L-T3).
    pub fn display(&self) -> String {
        match self {
            ComposerEntry::Text(t) => t.clone(),
            ComposerEntry::Chip { line_count, .. } => format!("[pasted: {line_count} lines]"),
        }
    }

    /// The text the model receives — chips expand to their full payload.
    pub fn expanded(&self) -> &str {
        match self {
            ComposerEntry::Text(t) => t,
            ComposerEntry::Chip { full_text, .. } => full_text,
        }
    }
}

/// Where a slash command comes from — decides the glyph the menu row shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SlashKind {
    /// Productized: shipped by stella itself (🔒).
    #[default]
    Builtin,
    /// Custom: a user-authored command/skill definition loaded from the
    /// workspace or user-global extension directories (⚡).
    Custom,
}

impl SlashKind {
    /// The menu-row glyph: 🔒 for productized commands, ⚡ for custom ones.
    pub fn glyph(self) -> &'static str {
        match self {
            SlashKind::Builtin => "🔒",
            SlashKind::Custom => "⚡",
        }
    }
}

/// A single slash command offered by the menu. The `name` includes the
/// leading slash (e.g. `"/help"`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashCommand {
    pub name: String,
    pub description: String,
    pub kind: SlashKind,
}

impl SlashCommand {
    /// A productized (built-in) command — the 🔒 rows.
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            kind: SlashKind::Builtin,
        }
    }

    /// A custom command/skill loaded from a definition file — the ⚡ rows.
    pub fn custom(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            kind: SlashKind::Custom,
            ..Self::new(name, description)
        }
    }
}

/// The input-line model. Committed paste chips precede the live text buffer;
/// what the user is currently typing lives in `buffer`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Composer {
    /// Chips committed ahead of the live buffer, in order.
    chips: Vec<ComposerEntry>,
    /// The text currently being typed.
    buffer: String,
    /// Paste-collapse threshold in lines.
    paste_threshold: usize,
}

impl Composer {
    /// A composer with the default paste threshold.
    pub fn new() -> Self {
        Self {
            paste_threshold: DEFAULT_PASTE_LINE_THRESHOLD,
            ..Self::default()
        }
    }

    /// A composer with an explicit paste threshold.
    pub fn with_paste_threshold(threshold: usize) -> Self {
        Self {
            paste_threshold: threshold.max(1),
            ..Self::default()
        }
    }

    /// The live buffer text (what is being typed, chips excluded).
    pub fn buffer(&self) -> &str {
        &self.buffer
    }

    /// The committed chips ahead of the buffer.
    pub fn chips(&self) -> &[ComposerEntry] {
        &self.chips
    }

    /// True when there is nothing to submit.
    pub fn is_empty(&self) -> bool {
        self.chips.is_empty() && self.buffer.trim().is_empty()
    }

    /// Type one character into the live buffer.
    pub fn insert_char(&mut self, c: char) {
        self.buffer.push(c);
    }

    /// Delete the last character; if the buffer is empty, pop the last chip
    /// instead (backspacing off the front of the buffer removes a paste).
    pub fn backspace(&mut self) {
        if self.buffer.pop().is_none() {
            self.chips.pop();
        }
    }

    /// Handle a paste. A paste at or above the line threshold collapses to a
    /// chip (the full payload retained); a small paste inserts inline.
    pub fn paste(&mut self, pasted: &str) {
        let line_count = line_count(pasted);
        if line_count >= self.paste_threshold {
            // Flush the in-progress buffer as its own text entry so ordering
            // (typed text, then chip) is preserved on submit.
            if !self.buffer.is_empty() {
                self.chips
                    .push(ComposerEntry::Text(std::mem::take(&mut self.buffer)));
            }
            self.chips.push(ComposerEntry::Chip {
                full_text: pasted.to_string(),
                line_count,
            });
        } else {
            self.buffer.push_str(pasted);
        }
    }

    /// The on-screen input line: chip displays joined with the live buffer.
    pub fn display_line(&self) -> String {
        let mut parts: Vec<String> = self.chips.iter().map(ComposerEntry::display).collect();
        if !self.buffer.is_empty() || parts.is_empty() {
            parts.push(self.buffer.clone());
        }
        parts.join(" ")
    }

    /// Assemble the full message the model receives — chips expanded to their
    /// payloads — and clear the composer. Returns `None` when empty.
    pub fn take_submission(&mut self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let mut parts: Vec<String> = self
            .chips
            .iter()
            .map(|c| c.expanded().to_string())
            .collect();
        if !self.buffer.is_empty() {
            parts.push(std::mem::take(&mut self.buffer));
        }
        self.chips.clear();
        Some(parts.join("\n"))
    }

    /// Clear the composer without submitting.
    pub fn clear(&mut self) {
        self.chips.clear();
        self.buffer.clear();
    }

    /// Replace the composer's content with `text` — the queue editor uses
    /// this to pull a queued prompt back in for editing. Any in-progress
    /// chips/typing are discarded (the caller decides when that is right).
    pub fn load(&mut self, text: impl Into<String>) {
        self.chips.clear();
        self.buffer = text.into();
    }

    /// The slash-menu view over `commands`, or `None` when the buffer is not
    /// a slash query. Active only when the whole buffer is a single `/`-word
    /// (no spaces yet) with no committed chips.
    pub fn slash_menu<'a>(&self, commands: &'a [SlashCommand]) -> Option<SlashMenu<'a>> {
        if !self.chips.is_empty() {
            return None;
        }
        let q = self.buffer.as_str();
        if !q.starts_with('/') || q.contains(char::is_whitespace) {
            return None;
        }
        Some(SlashMenu::filter(commands, q))
    }
}

/// The filtered slash-command list for the current query. Borrows the
/// caller's command vocabulary — the menu owns no command list of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashMenu<'a> {
    pub query: String,
    pub matches: Vec<&'a SlashCommand>,
}

impl<'a> SlashMenu<'a> {
    /// Case-insensitive prefix filter over `commands`. An empty query (just
    /// `/`) matches everything.
    pub fn filter(commands: &'a [SlashCommand], query: &str) -> Self {
        let needle = query.to_ascii_lowercase();
        let matches = commands
            .iter()
            .filter(|c| c.name.to_ascii_lowercase().starts_with(&needle))
            .collect();
        Self {
            query: query.to_string(),
            matches,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.matches.is_empty()
    }
}

/// The names of the slash commands currently matching the composer, or empty
/// when the popup should be inactive. Owned strings so a caller can keep
/// mutating its own UI state while acting on them.
pub fn slash_popup_matches(composer: &Composer, slash_commands: &[SlashCommand]) -> Vec<String> {
    composer
        .slash_menu(slash_commands)
        .map(|m| m.matches.iter().map(|c| c.name.clone()).collect())
        .unwrap_or_default()
}

/// What a slash-popup key press should do, abstracted over the caller's own
/// action type — a REPL `Prompt` and a deck `Enqueue` both start from the
/// same submitted text.
pub enum SlashPopupOutcome {
    /// Navigation, completion, or dismiss — fully handled here.
    Handled,
    /// Enter: dispatch this text as a prompt.
    Submit(String),
}

/// Slash-popup navigation shared by every composer-driven surface: ↑/↓
/// choose, Tab completes into the buffer, Enter dispatches the selection,
/// Esc dismisses. Returns `None` for a key the popup doesn't claim, so the
/// caller can fall through to normal composer editing. `matches` must be
/// non-empty — callers only reach this once the popup is confirmed active.
pub fn handle_slash_popup_key(
    key: KeyEvent,
    matches: &[String],
    composer: &mut Composer,
    slash_selected: &mut usize,
) -> Option<SlashPopupOutcome> {
    let selected = (*slash_selected).min(matches.len() - 1);
    match key.code {
        KeyCode::Up => {
            *slash_selected = selected.saturating_sub(1);
            Some(SlashPopupOutcome::Handled)
        }
        KeyCode::Down => {
            *slash_selected = (selected + 1).min(matches.len() - 1);
            Some(SlashPopupOutcome::Handled)
        }
        KeyCode::Tab => {
            composer.load(matches[selected].clone());
            *slash_selected = 0;
            Some(SlashPopupOutcome::Handled)
        }
        KeyCode::Enter => {
            composer.clear();
            *slash_selected = 0;
            Some(SlashPopupOutcome::Submit(matches[selected].clone()))
        }
        KeyCode::Esc => {
            composer.clear();
            *slash_selected = 0;
            Some(SlashPopupOutcome::Handled)
        }
        _ => None,
    }
}

/// Count the lines in a pasted payload — the metric the chip threshold uses.
/// A trailing newline does not add a phantom empty line.
fn line_count(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    text.trim_end_matches('\n').split('\n').count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commands() -> Vec<SlashCommand> {
        vec![
            SlashCommand::new("/help", "show help"),
            SlashCommand::new("/clear", "clear the transcript"),
            SlashCommand::new("/models", "list models"),
            SlashCommand::new("/diff", "open the diff viewer"),
            SlashCommand::new("/files", "focus the files panel"),
        ]
    }

    #[test]
    fn small_paste_inserts_inline() {
        let mut c = Composer::with_paste_threshold(6);
        c.paste("one\ntwo\nthree");
        assert!(c.chips().is_empty());
        assert_eq!(c.buffer(), "one\ntwo\nthree");
    }

    #[test]
    fn large_paste_collapses_to_a_chip_but_keeps_the_payload() {
        let mut c = Composer::with_paste_threshold(3);
        let payload = "a\nb\nc\nd\ne";
        c.paste(payload);
        assert_eq!(c.chips().len(), 1);
        assert_eq!(c.display_line(), "[pasted: 5 lines]");
        // The full payload survives to submission.
        let msg = c.take_submission().unwrap();
        assert_eq!(msg, payload);
    }

    #[test]
    fn typed_text_before_a_chip_keeps_its_order_on_submit() {
        let mut c = Composer::with_paste_threshold(3);
        for ch in "review this: ".chars() {
            c.insert_char(ch);
        }
        c.paste("x\ny\nz\nw");
        for ch in " thanks".chars() {
            c.insert_char(ch);
        }
        let msg = c.take_submission().unwrap();
        assert_eq!(msg, "review this: \nx\ny\nz\nw\n thanks");
        assert!(c.is_empty(), "submission clears the composer");
    }

    #[test]
    fn display_never_leaks_the_raw_payload() {
        let mut c = Composer::with_paste_threshold(2);
        c.paste("secret-line-1\nsecret-line-2\nsecret-line-3");
        let shown = c.display_line();
        assert!(
            !shown.contains("secret"),
            "chip must hide the payload: {shown}"
        );
    }

    #[test]
    fn backspace_pops_a_chip_when_the_buffer_is_empty() {
        let mut c = Composer::with_paste_threshold(2);
        c.paste("a\nb\nc");
        assert_eq!(c.chips().len(), 1);
        c.backspace(); // buffer empty → removes the chip
        assert!(c.chips().is_empty());
    }

    #[test]
    fn slash_menu_filters_by_prefix() {
        let cmds = commands();
        let mut c = Composer::new();
        for ch in "/f".chars() {
            c.insert_char(ch);
        }
        let menu = c.slash_menu(&cmds).expect("slash menu active");
        let names: Vec<&str> = menu.matches.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, vec!["/files"]);
    }

    #[test]
    fn bare_slash_lists_every_command() {
        let cmds = commands();
        let mut c = Composer::new();
        c.insert_char('/');
        let menu = c.slash_menu(&cmds).unwrap();
        assert_eq!(menu.matches.len(), cmds.len());
    }

    #[test]
    fn slash_menu_is_inactive_once_a_space_is_typed() {
        let cmds = commands();
        let mut c = Composer::new();
        for ch in "/models ".chars() {
            c.insert_char(ch);
        }
        assert!(c.slash_menu(&cmds).is_none());
    }

    #[test]
    fn slash_command_constructors_set_the_kind() {
        assert_eq!(SlashCommand::new("/help", "d").kind, SlashKind::Builtin);
        assert_eq!(SlashCommand::custom("/x", "d").kind, SlashKind::Custom);
    }

    #[test]
    fn slash_menu_is_inactive_when_chips_are_present() {
        let cmds = commands();
        let mut c = Composer::with_paste_threshold(2);
        c.paste("a\nb\nc");
        c.insert_char('/');
        assert!(c.slash_menu(&cmds).is_none());
    }

    #[test]
    fn line_count_ignores_a_trailing_newline() {
        assert_eq!(line_count("a\nb\n"), 2);
        assert_eq!(line_count("a\nb"), 2);
        assert_eq!(line_count(""), 0);
        assert_eq!(line_count("solo"), 1);
    }
}
