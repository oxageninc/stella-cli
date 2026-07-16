//! Terminal enter/restore shared by both shells (single-session and deck).
//!
//! Restoration must not rely on `Drop` alone: release builds abort on panic
//! (`[profile.release] panic = "abort"`), so destructors never run on a
//! panicking process and a Drop-only guard leaves the user's terminal
//! stranded in raw mode on the alternate screen. The panic hook installed by
//! [`PanicHookGuard`] therefore restores the terminal *directly* — sharing
//! the guard's acquired-state flags — and then prints the panic to the real
//! screen.
//!
//! The one panic that must NOT tear the session down is a panel panic in a
//! dev (unwind) build: `render::guarded_panel` catches those and renders an
//! error card in place. Panel draws mark themselves via [`PanelBoundary`],
//! and the hook skips restoration for them — but only when unwinding is
//! actually available (`cfg!(panic = "unwind")`); under abort the catch is
//! inert, the process is about to die, and restoring is always right.

use std::cell::Cell;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

/// Which terminal states are currently held. Shared between the guard (for
/// Drop-time restore on the orderly path) and the panic hook (for abort-safe
/// restore from whichever thread panicked).
#[derive(Debug, Default)]
struct TermState {
    raw: AtomicBool,
    alt: AtomicBool,
    mouse: AtomicBool,
}

impl TermState {
    /// Roll back exactly the states that were acquired. Idempotent: the
    /// `swap(false)` means a hook-time restore leaves nothing for a later
    /// Drop-time restore to redo.
    fn restore(&self) {
        let mut out = io::stdout();
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
        if mouse {
            execute!(out, EnableMouseCapture)?;
            guard.state.mouse.store(true, Ordering::SeqCst);
        }
        Ok(guard)
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

/// RAII marker for a panel draw whose panics are caught by the caller.
/// While one is alive on a thread, the panic hook leaves the terminal alone
/// in unwind builds so the session can continue with an error card.
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

/// Installs a panic hook that restores the terminal before the process dies
/// and puts the previous hook back on drop.
///
/// The hook: (1) appends the panic to the structured debug log when one is
/// configured; (2) unless this is a caught panel panic in an unwind build,
/// restores the terminal via the shared state and prints the panic message —
/// which now lands on the user's real screen, not the alternate one. Under
/// `panic = "abort"` this hook is the ONLY restoration that ever runs on a
/// panicking process.
pub(crate) struct PanicHookGuard {
    prev: Option<PanicHook>,
}

impl PanicHookGuard {
    pub(crate) fn install(debug_log_path: Option<PathBuf>, terminal: &TerminalGuard) -> Self {
        let state = terminal.state.clone();
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if let Some(path) = &debug_log_path {
                let _ = crate::shell::append_json_line(
                    path,
                    "panic",
                    serde_json::json!({ "info": info.to_string() }),
                );
            }
            let caught_panel_panic =
                cfg!(panic = "unwind") && IN_GUARDED_PANEL.with(Cell::get);
            if !caught_panel_panic {
                state.restore();
                eprintln!("{info}");
            }
        }));
        Self { prev: Some(prev) }
    }
}

impl Drop for PanicHookGuard {
    fn drop(&mut self) {
        if let Some(prev) = self.prev.take() {
            std::panic::set_hook(prev);
        }
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
