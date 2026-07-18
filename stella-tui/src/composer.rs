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
//!
//! ## Textarea semantics
//!
//! The live buffer is a real multi-line editor: a **modified** `⏎` (`⌘⏎`/`⌃⏎`,
//! or the universally-safe `⌥⏎`) inserts a line break that survives verbatim
//! into the submitted prompt, the cursor moves freely (arrows, Home/End,
//! `⌥[`/`⌥]` to the very start/end), and [`layout`] soft-wraps the content to
//! the viewport width so everything typed stays visible before submitting. A
//! **bare** `⏎` always submits (never blocks) — see [`classify_enter`].

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_width::UnicodeWidthChar;

/// Below this many lines a paste is inserted inline; at or above it, the
/// paste collapses to a chip. Small on purpose (L-T3).
pub const DEFAULT_PASTE_LINE_THRESHOLD: usize = 6;

/// The deck composer's paste threshold. The deck's input box grows to a few
/// lines and then *scrolls* (see `DECK_COMPOSER_MAX_ROWS`), so a normal
/// multi-line prompt should render inline — one `>>>` per line — rather than
/// collapse to a chip; only a genuinely huge blob (a whole file) is worth
/// chipping to protect the model context. This sits well past the visible cap.
pub const DECK_PASTE_LINE_THRESHOLD: usize = 48;

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

/// The input model. Committed paste chips precede the live text buffer;
/// what the user is currently typing lives in `buffer`, a multi-line
/// textarea with a movable cursor.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Composer {
    /// Chips committed ahead of the live buffer, in order.
    chips: Vec<ComposerEntry>,
    /// The text currently being typed. May contain `\n` — line breaks are
    /// preserved verbatim through [`Composer::take_submission`].
    buffer: String,
    /// Byte offset of the cursor within `buffer` (always on a char boundary).
    cursor: usize,
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

    /// True when there is nothing at all to edit — no chips and not even
    /// whitespace in the buffer (stricter than [`Composer::is_empty`], which
    /// is about submittability).
    pub fn is_blank(&self) -> bool {
        self.chips.is_empty() && self.buffer.is_empty()
    }

    /// Byte offset of the cursor within [`Composer::buffer`].
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Type one character at the cursor.
    pub fn insert_char(&mut self, c: char) {
        self.buffer.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    /// Insert a line break at the cursor — the modified-`⏎` textarea action
    /// (`⌘⏎`/`⌃⏎`/`⌥⏎`; a bare `⏎` submits instead).
    pub fn insert_newline(&mut self) {
        self.insert_char('\n');
    }

    /// Delete the character before the cursor; at the very start of the
    /// buffer, pop the last chip instead (backspacing off the front of the
    /// buffer removes a paste).
    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            let prev = prev_char_start(&self.buffer, self.cursor);
            self.buffer.replace_range(prev..self.cursor, "");
            self.cursor = prev;
        } else {
            self.chips.pop();
        }
    }

    /// Handle a paste at the cursor. A paste at or above the line threshold
    /// collapses to a chip (the full payload retained); a small paste inserts
    /// inline. Terminal paste streams carry `\r`/`\r\n` line endings in raw
    /// mode — normalized to `\n` so the buffer has one newline convention.
    pub fn paste(&mut self, pasted: &str) {
        let pasted = pasted.replace("\r\n", "\n").replace('\r', "\n");
        let line_count = line_count(&pasted);
        if line_count >= self.paste_threshold {
            // Text before the cursor is committed ahead of the chip so
            // ordering (typed text, then chip) is preserved on submit; text
            // after the cursor stays in the buffer, which follows the chips.
            let before = self.buffer[..self.cursor].to_string();
            let after = self.buffer[self.cursor..].to_string();
            if !before.is_empty() {
                self.chips.push(ComposerEntry::Text(before));
            }
            self.chips.push(ComposerEntry::Chip {
                full_text: pasted,
                line_count,
            });
            self.buffer = after;
            self.cursor = 0;
        } else {
            self.buffer.insert_str(self.cursor, &pasted);
            self.cursor += pasted.len();
        }
    }

    // ---- Cursor motion (textarea semantics) --------------------------------

    /// One character left.
    pub fn move_left(&mut self) {
        if self.cursor > 0 {
            self.cursor = prev_char_start(&self.buffer, self.cursor);
        }
    }

    /// One character right.
    pub fn move_right(&mut self) {
        if let Some(c) = self.buffer[self.cursor..].chars().next() {
            self.cursor += c.len_utf8();
        }
    }

    /// To the very start of the prompt — the `⌥[` jump (position 0, before
    /// the first character).
    pub fn move_to_start(&mut self) {
        self.cursor = 0;
    }

    /// To one past the last character — the `⌥]` jump.
    pub fn move_to_end(&mut self) {
        self.cursor = self.buffer.len();
    }

    /// To the start of the current logical line.
    pub fn move_line_start(&mut self) {
        self.cursor = line_start(&self.buffer, self.cursor);
    }

    /// To the end of the current logical line (before its `\n`).
    pub fn move_line_end(&mut self) {
        self.cursor = self.buffer[self.cursor..]
            .find('\n')
            .map(|i| self.cursor + i)
            .unwrap_or(self.buffer.len());
    }

    /// Up one logical line, keeping the character column where possible.
    /// On the first line, jumps to the start (matching most editors' clamp).
    pub fn move_up(&mut self) {
        let start = line_start(&self.buffer, self.cursor);
        if start == 0 {
            self.cursor = 0;
            return;
        }
        let col = self.buffer[start..self.cursor].chars().count();
        let prev_start = line_start(&self.buffer, start - 1);
        let prev_line = &self.buffer[prev_start..start - 1];
        self.cursor = prev_start + byte_at_char_col(prev_line, col);
    }

    /// Down one logical line, keeping the character column where possible.
    /// On the last line, jumps to the end.
    pub fn move_down(&mut self) {
        let Some(newline) = self.buffer[self.cursor..].find('\n') else {
            self.cursor = self.buffer.len();
            return;
        };
        let start = line_start(&self.buffer, self.cursor);
        let col = self.buffer[start..self.cursor].chars().count();
        let next_start = self.cursor + newline + 1;
        let next_end = self.buffer[next_start..]
            .find('\n')
            .map(|i| next_start + i)
            .unwrap_or(self.buffer.len());
        let next_line = &self.buffer[next_start..next_end];
        self.cursor = next_start + byte_at_char_col(next_line, col);
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
    /// payloads, typed line breaks preserved verbatim — and clear the
    /// composer. Returns `None` when empty.
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
        self.cursor = 0;
        Some(parts.join("\n"))
    }

    /// Clear the composer without submitting.
    pub fn clear(&mut self) {
        self.chips.clear();
        self.buffer.clear();
        self.cursor = 0;
    }

    /// Replace the composer's content with `text` — the queue editor uses
    /// this to pull a queued prompt back in for editing. Any in-progress
    /// chips/typing are discarded (the caller decides when that is right).
    /// The cursor lands at the end, ready to keep typing.
    pub fn load(&mut self, text: impl Into<String>) {
        self.chips.clear();
        self.buffer = text.into();
        self.cursor = self.buffer.len();
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

/// Byte index of the char boundary immediately before `idx`.
fn prev_char_start(s: &str, idx: usize) -> usize {
    s[..idx].char_indices().last().map(|(i, _)| i).unwrap_or(0)
}

/// Byte index where the logical line containing `idx` starts.
fn line_start(s: &str, idx: usize) -> usize {
    s[..idx].rfind('\n').map(|i| i + 1).unwrap_or(0)
}

/// Byte offset of character column `col` within `line`, clamped to its end.
fn byte_at_char_col(line: &str, col: usize) -> usize {
    line.char_indices()
        .nth(col)
        .map(|(i, _)| i)
        .unwrap_or(line.len())
}

// ---------------------------------------------------------------------------
// Enter classification + shared textarea key handling
// ---------------------------------------------------------------------------

/// What an `⏎` keypress means for the composer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnterAction {
    /// Dispatch the composer's content.
    Submit,
    /// Insert a line break at the cursor.
    Newline,
    /// The key is not Enter at all.
    NotEnter,
}

/// Classify an Enter keypress for a textarea composer.
///
/// One rule, honest across every terminal: a **bare** `⏎` submits, and `⏎` with
/// any newline modifier — `⌘⏎`/`⌃⏎` (macOS Cmd reports as SUPER/META) or `⌥⏎` —
/// inserts a line break. With the kitty keyboard protocol all three modifiers
/// are reportable; on a legacy terminal only `⌥⏎` survives (its ESC prefix
/// does), and an unreportable `⌘⏎`/`⌃⏎` harmlessly folds into a plain `⏎` and
/// submits — the best a legacy terminal can do. This is the inverse of the old
/// chord-to-submit mapping: Enter now always dispatches, so the queue is one
/// keystroke away and never blocks.
pub fn classify_enter(key: &KeyEvent) -> EnterAction {
    if !matches!(key.code, KeyCode::Enter) {
        return EnterAction::NotEnter;
    }
    let newline = key.modifiers.intersects(
        KeyModifiers::SUPER | KeyModifiers::META | KeyModifiers::CONTROL | KeyModifiers::ALT,
    );
    if newline {
        EnterAction::Newline
    } else {
        EnterAction::Submit
    }
}

/// Textarea cursor-motion keys shared by every composer surface. Returns
/// `true` when the key was consumed. Motion that would collide with a
/// surface's own navigation (transcript scroll, tab views) is gated on the
/// buffer actually having something to move through: ←/→ need text, ↑/↓ need
/// a second line — so an empty composer leaves every arrow to its surface.
pub fn handle_edit_key(key: KeyEvent, composer: &mut Composer) -> bool {
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let cmd = key
        .modifiers
        .intersects(KeyModifiers::SUPER | KeyModifiers::META);
    let has_text = !composer.buffer().is_empty();
    let multiline = composer.buffer().contains('\n');
    match key.code {
        // ⌥[ / ⌥] — cursor (and the wrapped view with it) to the very start /
        // one past the last character.
        KeyCode::Char('[') if alt => composer.move_to_start(),
        KeyCode::Char(']') if alt => composer.move_to_end(),
        // ⌘↑ / ⌘↓ — the macOS-native start/end-of-document synonyms.
        KeyCode::Up if cmd => composer.move_to_start(),
        KeyCode::Down if cmd => composer.move_to_end(),
        KeyCode::Left if has_text => composer.move_left(),
        KeyCode::Right if has_text => composer.move_right(),
        KeyCode::Up if multiline => composer.move_up(),
        KeyCode::Down if multiline => composer.move_down(),
        KeyCode::Home if has_text => composer.move_line_start(),
        KeyCode::End if has_text => composer.move_line_end(),
        _ => return false,
    }
    true
}

// ---------------------------------------------------------------------------
// Soft-wrap layout (pure, so both renderers and the tests share one truth)
// ---------------------------------------------------------------------------

/// The composer soft-wrapped to a viewport width: every visual row plus the
/// cursor's position among them. Hard breaks (`\n`) and soft wraps both
/// produce rows, so `rows.len()` is the height the composer wants and the
/// caller can scroll a capped window to `cursor_row`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposerLayout {
    /// The wrapped display rows (chips rendered as their `[pasted: …]` form).
    pub rows: Vec<String>,
    /// Row index the cursor sits on.
    pub cursor_row: usize,
    /// Display-width column of the cursor within `rows[cursor_row]`.
    pub cursor_col: usize,
}

/// Soft-wrap the composer's display content to `width` columns
/// (unicode-width aware; `\n` is a hard break) and locate the cursor.
pub fn layout(composer: &Composer, width: usize) -> ComposerLayout {
    let width = width.max(1);
    let mut display = String::new();
    for chip in &composer.chips {
        display.push_str(&chip.display());
        display.push(' ');
    }
    let cursor_at = display.len() + composer.cursor();
    display.push_str(composer.buffer());

    let mut rows: Vec<String> = vec![String::new()];
    let mut col = 0usize;
    let (mut cursor_row, mut cursor_col) = (0usize, 0usize);
    for (idx, ch) in display.char_indices() {
        if ch == '\n' {
            // A cursor on the newline itself renders at this row's end.
            if idx == cursor_at {
                (cursor_row, cursor_col) = (rows.len() - 1, col);
            }
            rows.push(String::new());
            col = 0;
            continue;
        }
        let w = ch.width().unwrap_or(0);
        if col + w > width && col > 0 {
            rows.push(String::new());
            col = 0;
        }
        if idx == cursor_at {
            (cursor_row, cursor_col) = (rows.len() - 1, col);
        }
        rows.last_mut().expect("rows is never empty").push(ch);
        col += w;
    }
    if cursor_at == display.len() {
        // Cursor past the last character; if that row is exactly full the
        // insertion point visually lives on a fresh row.
        if col >= width {
            rows.push(String::new());
            col = 0;
        }
        (cursor_row, cursor_col) = (rows.len() - 1, col);
    }
    ComposerLayout {
        rows,
        cursor_row,
        cursor_col,
    }
}

/// Split one display row at `col` display columns for block-cursor drawing:
/// `(before, under, after)`, where `under` is the character the cursor sits
/// on (`None` at end of row — the caller draws a reversed space).
pub fn split_row_at(row: &str, col: usize) -> (String, Option<char>, String) {
    let mut acc = 0usize;
    let mut chars = row.chars();
    let mut before = String::new();
    for ch in chars.by_ref() {
        if acc >= col {
            return (before, Some(ch), chars.collect());
        }
        acc += ch.width().unwrap_or(0);
        before.push(ch);
    }
    (before, None, String::new())
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

    // ---- Textarea semantics -------------------------------------------------

    fn typed(text: &str) -> Composer {
        let mut c = Composer::new();
        for ch in text.chars() {
            c.insert_char(ch);
        }
        c
    }

    #[test]
    fn newlines_typed_into_the_buffer_survive_submission_verbatim() {
        let mut c = typed("first line");
        c.insert_newline();
        for ch in "second line".chars() {
            c.insert_char(ch);
        }
        assert_eq!(c.take_submission().unwrap(), "first line\nsecond line");
    }

    #[test]
    fn insert_and_backspace_act_at_the_cursor() {
        let mut c = typed("hello");
        c.move_left();
        c.move_left();
        c.insert_char('X'); // hel X lo
        assert_eq!(c.buffer(), "helXlo");
        c.backspace(); // removes the X, not the tail
        assert_eq!(c.buffer(), "hello");
        assert_eq!(c.cursor(), 3);
    }

    #[test]
    fn move_to_start_and_end_bound_the_whole_buffer() {
        let mut c = typed("a\nb\nc");
        c.move_to_start();
        assert_eq!(c.cursor(), 0, "before the first character");
        c.move_to_end();
        assert_eq!(c.cursor(), c.buffer().len(), "one past the last character");
    }

    #[test]
    fn vertical_motion_keeps_the_column_and_clamps_to_short_lines() {
        let mut c = typed("long line\nab\nlonger line");
        // Cursor at end of "longer line"; up lands clamped to "ab"'s end.
        c.move_up();
        assert_eq!(&c.buffer()[..c.cursor()], "long line\nab");
        // Up again: column carried from the clamp point (2) into "long line".
        c.move_up();
        assert_eq!(&c.buffer()[..c.cursor()], "lo");
        // Down from the first line's column 2 → "ab" clamps to its end again.
        c.move_down();
        assert_eq!(&c.buffer()[..c.cursor()], "long line\nab");
        // Down on the last line jumps to the very end.
        c.move_down();
        c.move_down();
        assert_eq!(c.cursor(), c.buffer().len());
    }

    #[test]
    fn line_start_and_end_stay_within_the_logical_line() {
        let mut c = typed("one\ntwo three");
        // Cursor at end; Home goes to the start of "two three", not offset 0.
        c.move_line_start();
        assert_eq!(&c.buffer()[..c.cursor()], "one\n");
        c.move_line_end();
        assert_eq!(c.cursor(), c.buffer().len());
    }

    #[test]
    fn paste_lands_at_the_cursor_and_normalizes_line_endings() {
        let mut c = typed("ac");
        c.move_left();
        c.paste("b\r\nB"); // small paste: inline, CRLF → LF
        assert_eq!(c.buffer(), "ab\nBc");
    }

    #[test]
    fn big_paste_mid_buffer_keeps_the_tail_after_the_chip() {
        let mut c = Composer::with_paste_threshold(2);
        for ch in "headtail".chars() {
            c.insert_char(ch);
        }
        for _ in 0..4 {
            c.move_left(); // cursor between "head" and "tail"
        }
        c.paste("x\ny\nz");
        let msg = c.take_submission().unwrap();
        assert_eq!(msg, "head\nx\ny\nz\ntail", "order: before, chip, after");
    }

    #[test]
    fn classify_enter_submits_bare_and_breaks_on_a_modifier() {
        let plain = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let cmd = KeyEvent::new(KeyCode::Enter, KeyModifiers::SUPER);
        let meta = KeyEvent::new(KeyCode::Enter, KeyModifiers::META);
        let ctrl = KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL);
        let alt = KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT);
        // Bare Enter always submits (never blocks).
        assert_eq!(classify_enter(&plain), EnterAction::Submit);
        // Every newline modifier inserts a line break instead.
        assert_eq!(classify_enter(&cmd), EnterAction::Newline);
        assert_eq!(classify_enter(&meta), EnterAction::Newline);
        assert_eq!(classify_enter(&ctrl), EnterAction::Newline);
        assert_eq!(classify_enter(&alt), EnterAction::Newline);
        // A non-Enter key is not this function's concern.
        let other = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        assert_eq!(classify_enter(&other), EnterAction::NotEnter);
    }

    #[test]
    fn edit_keys_leave_an_empty_composer_to_the_surface() {
        // Arrows on an empty buffer must fall through (they scroll views).
        let mut c = Composer::new();
        for code in [KeyCode::Left, KeyCode::Right, KeyCode::Up, KeyCode::Down] {
            assert!(!handle_edit_key(
                KeyEvent::new(code, KeyModifiers::NONE),
                &mut c
            ));
        }
        // ↑/↓ on a single-line buffer also fall through (transcript scroll).
        let mut single = typed("one line");
        assert!(!handle_edit_key(
            KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
            &mut single
        ));
        assert!(handle_edit_key(
            KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
            &mut single
        ));
    }

    // ---- Soft-wrap layout ---------------------------------------------------

    #[test]
    fn layout_soft_wraps_long_lines_and_hard_breaks_newlines() {
        let c = typed("abcdef\ngh");
        let l = layout(&c, 4);
        assert_eq!(l.rows, vec!["abcd", "ef", "gh"]);
        // Cursor at the end: one past 'h' on the last row.
        assert_eq!((l.cursor_row, l.cursor_col), (2, 2));
    }

    #[test]
    fn layout_places_the_cursor_mid_text() {
        let mut c = typed("abcdef");
        for _ in 0..2 {
            c.move_left(); // cursor before 'e' (offset 4)
        }
        let l = layout(&c, 4);
        assert_eq!(l.rows, vec!["abcd", "ef"]);
        assert_eq!((l.cursor_row, l.cursor_col), (1, 0), "'e' starts row 1");
    }

    #[test]
    fn layout_gives_the_cursor_a_fresh_row_when_the_last_row_is_full() {
        let c = typed("abcd");
        let l = layout(&c, 4);
        assert_eq!(l.rows, vec!["abcd", ""]);
        assert_eq!((l.cursor_row, l.cursor_col), (1, 0));
    }

    #[test]
    fn layout_shows_chips_as_their_display_form() {
        let mut c = Composer::with_paste_threshold(2);
        c.paste("a\nb\nc");
        for ch in "ok".chars() {
            c.insert_char(ch);
        }
        let l = layout(&c, 40);
        assert_eq!(l.rows, vec!["[pasted: 3 lines] ok"]);
        assert_eq!((l.cursor_row, l.cursor_col), (0, 20));
    }

    #[test]
    fn layout_is_wide_char_aware() {
        let c = typed("日本語"); // width 2 each
        let l = layout(&c, 4);
        assert_eq!(l.rows, vec!["日本", "語"]);
        assert_eq!((l.cursor_row, l.cursor_col), (1, 2));
    }

    #[test]
    fn split_row_at_returns_the_char_under_the_cursor() {
        assert_eq!(split_row_at("abc", 1), ("a".into(), Some('b'), "c".into()));
        assert_eq!(split_row_at("abc", 3), ("abc".into(), None, String::new()));
        assert_eq!(
            split_row_at("日本", 2),
            ("日".into(), Some('本'), String::new())
        );
    }

    #[test]
    fn empty_composer_lays_out_as_one_empty_row_with_the_cursor_home() {
        let c = Composer::new();
        let l = layout(&c, 10);
        assert_eq!(l.rows, vec![""]);
        assert_eq!((l.cursor_row, l.cursor_col), (0, 0));
    }
}
