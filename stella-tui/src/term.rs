//! Terminal enter/restore shared by both shells (single-session and deck).
//!
//! Restoration must not rely on `Drop` alone: release builds abort on panic
//! (`[profile.release] panic = "abort"`), so destructors never run on a
//! panicking process and a Drop-only guard leaves the user's terminal
//! stranded in raw mode on the alternate screen. The panic hook installed by
//! [`PanicHookGuard`] therefore restores the terminal *directly* in abort
//! builds — sharing the guard's acquired-state flags — and then delegates to
//! the previously installed hook, so the standard panic message (and any
//! `RUST_BACKTRACE` output) lands on the user's real screen, not the
//! alternate one.
//!
//! In unwind (dev) builds the hook never restores; it only writes the debug
//! log. The hook fires for EVERY panic on ANY thread or tokio task, and a
//! session can outlive a panic in two ways that make hook-time restoration
//! actively harmful: panel panics are caught by `render::guarded_panel` and
//! rendered as an error card in place (see [`PanelBoundary`]), and a panic
//! on a background task can leave the deck session running while other
//! sessions continue — restoring there would tear the live UI out from
//! under them. Any panic that really does end the program unwinds into
//! [`TerminalGuard`]'s `Drop`, which performs the restore on that orderly
//! path. Under abort none of that machinery gets a chance to run — the
//! process dies inside the hook — so the hook is the only restoration point
//! and restoring is always right, even mid-panel-draw.

use std::cell::Cell;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    supports_keyboard_enhancement,
};

/// Which terminal states are currently held. Shared between the guard (for
/// Drop-time restore on the orderly path) and the panic hook (for abort-safe
/// restore from whichever thread panicked).
#[derive(Debug, Default)]
struct TermState {
    raw: AtomicBool,
    alt: AtomicBool,
    mouse: AtomicBool,
    paste: AtomicBool,
    kitty: AtomicBool,
}

impl TermState {
    /// Roll back exactly the states that were acquired. Idempotent: the
    /// `swap(false)` means a hook-time restore leaves nothing for a later
    /// Drop-time restore to redo.
    fn restore(&self) {
        let mut out = io::stdout();
        if self.kitty.swap(false, Ordering::SeqCst) {
            let _ = execute!(out, PopKeyboardEnhancementFlags);
        }
        if self.paste.swap(false, Ordering::SeqCst) {
            let _ = execute!(out, DisableBracketedPaste);
        }
        if self.mouse.swap(false, Ordering::SeqCst) {
            let _ = execute!(out, DisableMouseCapture);
        }
        if self.alt.swap(false, Ordering::SeqCst) {
            let _ = execute!(out, LeaveAlternateScreen);
        }
        if self.raw.swap(false, Ordering::SeqCst) {
            let _ = disable_raw_mode();
        }
    }
}

/// Restores the terminal (raw mode + alternate screen, and mouse capture if
/// it was enabled) on drop — and, via [`PanicHookGuard`], on panic even in
/// abort builds.
///
/// Each terminal state is flagged as it is acquired, and the guard exists
/// BEFORE the first acquisition — so an error partway through `enter` (raw
/// mode on, alternate screen failed) still drops the guard and rolls back
/// exactly the states that were entered, never stranding the user's
/// terminal in raw mode.
pub(crate) struct TerminalGuard {
    state: Arc<TermState>,
}

impl TerminalGuard {
    pub(crate) fn enter(mouse: bool) -> io::Result<Self> {
        let guard = Self {
            state: Arc::new(TermState::default()),
        };
        let mut out = io::stdout();
        enable_raw_mode()?;
        guard.state.raw.store(true, Ordering::SeqCst);
        execute!(out, EnterAlternateScreen)?;
        guard.state.alt.store(true, Ordering::SeqCst);
        // Always on: without bracketed paste a multi-line paste arrives as
        // raw key events, so every newline acts as Enter — one paste becomes
        // N separate prompt submissions. With it, the whole paste arrives as
        // one `Event::Paste` the composer folds into a chip.
        execute!(out, EnableBracketedPaste)?;
        guard.state.paste.store(true, Ordering::SeqCst);
        if mouse {
            execute!(out, EnableMouseCapture)?;
            guard.state.mouse.store(true, Ordering::SeqCst);
        }
        // The kitty keyboard protocol disambiguates modified keys, letting
        // `⌘⏎`/`⌃⏎` submit while plain `⏎` inserts a line break. Probing
        // needs raw mode, so this comes last; best-effort (`false` on any
        // probe error → legacy Enter semantics).
        if matches!(supports_keyboard_enhancement(), Ok(true)) {
            execute!(
                out,
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
            )?;
            guard.state.kitty.store(true, Ordering::SeqCst);
        }
        Ok(guard)
    }

    /// Whether the kitty keyboard protocol was pushed. When it is active, a
    /// modified Enter (`⌘⏎`/`⌃⏎`) is reportable and the composer runs full
    /// textarea semantics; without it the shell falls back to Enter-submits
    /// (see `crate::composer::classify_enter`).
    pub(crate) fn kitty(&self) -> bool {
        self.state.kitty.load(Ordering::SeqCst)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.state.restore();
    }
}

thread_local! {
    /// True while the current thread is inside a `guarded_panel` draw whose
    /// panics are caught and rendered in place (dev builds only — see the
    /// module docs for the abort-build caveat).
    static IN_GUARDED_PANEL: Cell<bool> = const { Cell::new(false) };
}

/// RAII marker for a panel draw whose panics are caught by the caller
/// (`render::guarded_panel`, effective in unwind builds only).
///
/// The panic hook no longer consults this flag: in unwind builds the hook
/// never restores the terminal at all (module docs), and under abort the
/// catch is inert — the process is about to die — so restoring even during
/// a marked panel draw is always right. The marker is kept as the
/// panel-draw-in-progress mechanism (and for anything that later needs to
/// distinguish caught-in-place panics from fatal ones).
pub(crate) struct PanelBoundary {
    prev: bool,
}

impl PanelBoundary {
    pub(crate) fn enter() -> Self {
        let prev = IN_GUARDED_PANEL.with(|f| f.replace(true));
        Self { prev }
    }
}

impl Drop for PanelBoundary {
    fn drop(&mut self) {
        let prev = self.prev;
        IN_GUARDED_PANEL.with(|f| f.set(prev));
    }
}

/// The boxed panic-hook type `std::panic::take_hook` hands back — aliased so
/// the guard's field stays readable.
type PanicHook = Box<dyn Fn(&std::panic::PanicHookInfo<'_>) + Sync + Send + 'static>;

/// Installs a panic hook that, in abort builds, restores the terminal before
/// the process dies — and puts the previous hook back on drop.
///
/// The hook: (1) always appends the panic to the structured debug log when
/// one is configured; (2) under `panic = "abort"` ONLY, restores the
/// terminal via the shared state and then delegates to the previously
/// installed hook, so the standard panic message and `RUST_BACKTRACE`
/// output print on the user's real screen, not the alternate one. In unwind
/// builds step (2) never happens — the hook fires for panics the session
/// survives (caught panel draws, background tasks), and real teardown
/// restores via [`TerminalGuard`]'s `Drop` instead (see the module docs).
pub(crate) struct PanicHookGuard {
    prev: Arc<PanicHook>,
}

impl PanicHookGuard {
    pub(crate) fn install(debug_log_path: Option<PathBuf>, terminal: &TerminalGuard) -> Self {
        let state = terminal.state.clone();
        // Shared with the installed closure so Drop can also reinstall it:
        // the closure delegates to the previous hook for the standard
        // message + backtrace, and the guard hands it back on teardown.
        let prev: Arc<PanicHook> = Arc::new(std::panic::take_hook());
        let delegate = Arc::clone(&prev);
        std::panic::set_hook(Box::new(move |info| {
            if let Some(path) = &debug_log_path {
                let _ = crate::shell::append_json_line(
                    path,
                    "panic",
                    serde_json::json!({ "info": info.to_string() }),
                );
            }
            // Abort builds only: the process dies right after this hook, so
            // it is the last chance to leave the terminal usable. Unwind
            // builds must NOT restore here — the panic may be one the
            // session survives (see the module docs).
            if cfg!(panic = "abort") {
                state.restore();
                (*delegate)(info);
            }
        }));
        Self { prev }
    }
}

impl Drop for PanicHookGuard {
    fn drop(&mut self) {
        let prev = Arc::clone(&self.prev);
        std::panic::set_hook(Box::new(move |info| (*prev)(info)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panel_boundary_flag_nests_and_resets() {
        assert!(!IN_GUARDED_PANEL.with(Cell::get));
        {
            let _outer = PanelBoundary::enter();
            assert!(IN_GUARDED_PANEL.with(Cell::get));
            {
                let _inner = PanelBoundary::enter();
                assert!(IN_GUARDED_PANEL.with(Cell::get));
            }
            assert!(
                IN_GUARDED_PANEL.with(Cell::get),
                "inner drop must restore the outer boundary, not clear it"
            );
        }
        assert!(!IN_GUARDED_PANEL.with(Cell::get));
    }

    #[test]
    fn term_state_restore_is_idempotent() {
        // No terminal is entered here: with all flags false, restore must be
        // a no-op both times (the swap(false) discipline), so calling it from
        // the panic hook and then again from Drop never double-emits escape
        // sequences.
        let state = TermState::default();
        state.restore();
        state.restore();
        assert!(!state.raw.load(Ordering::SeqCst));
        assert!(!state.alt.load(Ordering::SeqCst));
        assert!(!state.mouse.load(Ordering::SeqCst));
    }
}
