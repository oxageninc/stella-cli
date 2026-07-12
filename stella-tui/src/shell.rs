//! The interactive shell: terminal setup/teardown, the crossterm event loop,
//! and channel plumbing. **Deliberately thin** — every decision (key→action,
//! event→state) lives in the pure, unit-tested layers ([`crate::ui`],
//! [`crate::model`], [`crate::render`]); this file only wires them to real I/O
//! and is the one part not covered by unit tests (its integration smoke test
//! is `#[ignore]`d because it needs a TTY).
//!
//! ## Terminal restoration & signals (L-L1, L-T2)
//!
//! [`TerminalGuard`] enables raw mode + the alternate screen on entry and
//! **always** restores them on drop — including during a panic unwind, so a
//! crash never leaves the user's terminal wedged. Mouse capture is **off by
//! default** (L-T2): native terminal text selection/copy keeps working; it is
//! opt-in via [`RunOptions::mouse_capture`].
//!
//! In raw mode Ctrl-C is delivered as a *key event*, not a `SIGINT`, so we
//! handle it in-band as a clean cancel + quit — we own no native library locks
//! here, so there is nothing to drain and no signal to re-raise (the L-L1
//! re-raise discipline applies to the engine/context runtimes that hold
//! SQLite/ONNX handles, not to this render loop). A genuine `SIGINT` received
//! while *not* in raw mode keeps its default disposition.

use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use stella_protocol::AgentEvent;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::composer::{Composer, SlashCommand};
use crate::input::UserInput;
use crate::model::SessionModel;
use crate::render::render;
use crate::ui::{ShellAction, UiState, handle_key, ingest};

/// Configuration for one interactive session.
#[derive(Debug, Clone)]
pub struct RunOptions {
    /// The slash-command vocabulary shown by the composer's `/` menu.
    pub slash_commands: Vec<SlashCommand>,
    /// Line threshold above which a paste collapses to a chip (L-T3).
    pub paste_line_threshold: usize,
    /// When `Some`, structured event/log lines are appended here (L-T8). The
    /// CLI wires the real `~/.local/state/stella/logs/` path when
    /// `OXAGEN_DEBUG=1`; taking it as an option keeps this crate decoupled
    /// from that location.
    pub debug_log_path: Option<PathBuf>,
    /// Enable mouse capture. **Off by default** so native text selection
    /// keeps working (L-T2) — turn on only for an explicitly mouse-driven,
    /// keyboard-degradable feature.
    pub mouse_capture: bool,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            slash_commands: Vec::new(),
            paste_line_threshold: crate::composer::DEFAULT_PASTE_LINE_THRESHOLD,
            debug_log_path: None,
            mouse_capture: false,
        }
    }
}

/// A best-effort structured debug log (L-T8). Never panics and never fails the
/// TUI on an IO error — a lost log line must never take down the session.
#[derive(Debug, Clone, Default)]
pub struct DebugLog {
    path: Option<PathBuf>,
}

impl DebugLog {
    /// A log that writes to `path`, or a no-op sink when `path` is `None`.
    pub fn new(path: Option<PathBuf>) -> Self {
        Self { path }
    }

    /// True when this log actually writes somewhere.
    pub fn is_active(&self) -> bool {
        self.path.is_some()
    }

    /// Record an inbound `AgentEvent`.
    pub fn event(&self, event: &AgentEvent) {
        let payload = serde_json::to_value(event).unwrap_or(serde_json::Value::Null);
        self.append("event", payload);
    }

    /// Record an outbound user input.
    pub fn input(&self, input: &UserInput) {
        self.append(
            "input",
            serde_json::json!({ "input": format!("{input:?}") }),
        );
    }

    /// Record a free-form note.
    pub fn note(&self, msg: &str) {
        self.append("note", serde_json::json!({ "msg": msg }));
    }

    fn append(&self, kind: &str, payload: serde_json::Value) {
        if let Some(path) = &self.path {
            let _ = append_json_line(path, kind, payload);
        }
    }
}

/// Append one structured JSON line to `path` (best-effort).
fn append_json_line(path: &PathBuf, kind: &str, payload: serde_json::Value) -> io::Result<()> {
    use std::io::Write;
    let ts_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let line = serde_json::json!({ "ts_ms": ts_ms, "kind": kind, "payload": payload });
    writeln!(file, "{line}")
}

/// Restores the terminal (raw mode + alternate screen, and mouse capture if it
/// was enabled) on drop — including during a panic unwind.
struct TerminalGuard {
    mouse: bool,
}

impl TerminalGuard {
    fn enter(mouse: bool) -> io::Result<Self> {
        enable_raw_mode()?;
        let mut out = io::stdout();
        execute!(out, EnterAlternateScreen)?;
        if mouse {
            execute!(out, EnableMouseCapture)?;
        }
        Ok(Self { mouse })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut out = io::stdout();
        if self.mouse {
            let _ = execute!(out, DisableMouseCapture);
        }
        let _ = execute!(out, LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

/// The boxed panic-hook type `std::panic::take_hook` hands back — aliased so
/// the guard's field stays readable.
type PanicHook = Box<dyn Fn(&std::panic::PanicHookInfo<'_>) + Sync + Send + 'static>;

/// Installs a panic hook that routes panic info to the debug log instead of
/// splattering the alternate screen, and restores the previous hook on drop.
struct PanicHookGuard {
    prev: Option<PanicHook>,
}

impl PanicHookGuard {
    fn install(path: Option<PathBuf>) -> Self {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if let Some(path) = &path {
                let _ = append_json_line(
                    path,
                    "panic",
                    serde_json::json!({ "info": info.to_string() }),
                );
            }
            // Intentionally silent on stderr: we are on an alternate screen.
            // Panel panics are already caught by `guarded_panel`; anything
            // reaching this hook unwinds `run` and `TerminalGuard` restores
            // the screen on the way out.
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

/// Run the interactive TUI to completion.
///
/// `AgentEvent`s stream in over `events`; the user's [`UserInput`]s (prompt
/// submissions and scope-review decisions) stream out over `submissions`.
/// Returns when the engine closes the event stream, the input reader ends, or
/// the user quits (`q` from a panel / Ctrl-C), having always restored the
/// terminal first.
pub async fn run(
    opts: RunOptions,
    mut events: UnboundedReceiver<AgentEvent>,
    submissions: UnboundedSender<UserInput>,
) -> io::Result<()> {
    let debug = DebugLog::new(opts.debug_log_path.clone());
    debug.note("tui session start");

    // Order matters: declare the hook guard first so it drops *last* — the
    // terminal is restored before the panic hook is put back.
    let _hook_guard = PanicHookGuard::install(opts.debug_log_path.clone());
    let _term_guard = TerminalGuard::enter(opts.mouse_capture)?;

    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    let mut model = SessionModel::new();
    let mut ui = UiState::new(
        Composer::with_paste_threshold(opts.paste_line_threshold),
        opts.slash_commands,
    );

    // A blocking reader thread forwards crossterm input events to the async
    // loop. It polls so it can observe the shutdown flag and exit promptly.
    let (key_tx, mut key_rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let shutdown = Arc::new(AtomicBool::new(false));
    let reader_shutdown = shutdown.clone();
    let reader = std::thread::spawn(move || {
        while !reader_shutdown.load(Ordering::Relaxed) {
            match event::poll(std::time::Duration::from_millis(100)) {
                Ok(true) => match event::read() {
                    Ok(ev) => {
                        if key_tx.send(ev).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                },
                Ok(false) => {}
                Err(_) => break,
            }
        }
    });

    loop {
        terminal.draw(|f| render(&model, &mut ui, f))?;

        tokio::select! {
            maybe_event = events.recv() => {
                match maybe_event {
                    Some(ev) => {
                        debug.event(&ev);
                        ingest(&ev, &mut model, &mut ui);
                    }
                    // Engine closed the stream — the session is over.
                    None => break,
                }
            }
            maybe_key = key_rx.recv() => {
                match maybe_key {
                    Some(Event::Key(key)) if key.kind != KeyEventKind::Release => {
                        match handle_key(key, &model, &mut ui) {
                            ShellAction::Quit => {
                                debug.note("user quit");
                                let _ = submissions.send(UserInput::Cancel);
                                break;
                            }
                            ShellAction::Submit(input) => {
                                debug.input(&input);
                                let _ = submissions.send(input);
                            }
                            ShellAction::Handled | ShellAction::Ignored => {}
                        }
                    }
                    // Resize/paste/other events: the next draw picks them up.
                    Some(_) => {}
                    // Reader thread ended (stdin closed).
                    None => break,
                }
            }
        }
    }

    shutdown.store(true, Ordering::Relaxed);
    let _ = reader.join();
    debug.note("tui session end");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mouse_capture_is_off_by_default_for_native_copy() {
        // L-T2: enabling mouse tracking breaks the terminal's own selection —
        // the default must never do it.
        assert!(!RunOptions::default().mouse_capture);
    }

    #[test]
    fn debug_log_is_inactive_without_a_path() {
        let log = DebugLog::new(None);
        assert!(!log.is_active());
        // No path → no panic, no file, pure no-op.
        log.event(&AgentEvent::Complete {
            model: "glm".into(),
            cost_usd: 0.0,
        });
        log.note("nothing happens");
    }

    #[test]
    fn debug_log_appends_structured_event_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cli.output");
        let log = DebugLog::new(Some(path.clone()));
        assert!(log.is_active());
        log.event(&AgentEvent::Stage {
            name: stella_protocol::StageKind::Execute,
        });
        log.input(&UserInput::Prompt { text: "hi".into() });
        log.note("done");

        let contents = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 3, "one JSON line per record:\n{contents}");
        // Each line is valid JSON carrying the kind + a timestamp.
        for line in &lines {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            assert!(v.get("kind").is_some());
            assert!(v.get("ts_ms").is_some());
        }
        let kinds: Vec<String> = lines
            .iter()
            .map(|l| {
                serde_json::from_str::<serde_json::Value>(l).unwrap()["kind"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(kinds, vec!["event", "input", "note"]);
    }

    /// The one interactive path a unit test cannot drive: it needs a real TTY
    /// (raw mode + alternate screen). Documented and `#[ignore]`d per the
    /// task; run manually with `--ignored` on a terminal.
    #[tokio::test]
    #[ignore = "requires an interactive TTY (raw mode + alternate screen)"]
    async fn run_smoke_requires_a_tty() {
        let (ev_tx, ev_rx) = tokio::sync::mpsc::unbounded_channel();
        let (sub_tx, _sub_rx) = tokio::sync::mpsc::unbounded_channel();
        // Dropping the event sender immediately closes the stream, so `run`
        // returns as soon as it starts — proving the wiring, when a TTY is
        // present.
        drop(ev_tx);
        let _ = super::run(RunOptions::default(), ev_rx, sub_tx).await;
    }
}
