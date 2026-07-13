//! The deck's async run loop — the multi-agent analogue of [`crate::shell::run`].
//!
//! Deliberately thin, like the single-session shell: every decision
//! (key→action via [`crate::deck_ui`], event→state via
//! [`crate::deck_ui::ingest_inbound`], the frame via [`crate::deck_render`])
//! lives in pure, unit-tested layers. This file only wires them to real I/O.
//!
//! It differs from [`crate::shell::run`] in one structural way: a fixed
//! **animation/resource tick** (~30 fps) is a third `select!` arm. A live
//! dashboard — CPU gauges, elapsed timers, sparklines, tachyonfx transitions —
//! must repaint on a clock, not only when the agent streams. That tick is also
//! where the clock advances and the resource monitor samples, so all
//! time-based UI shares one heartbeat.

use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::composer::Composer;
use crate::deck::WorkspaceModel;
use crate::deck_render::render_deck;
use crate::deck_ui::{DeckAction, DeckUi, handle_deck_key, ingest_inbound};
use crate::envelope::{Inbound, WorkspaceInput};
use crate::graph::GraphSnapshot;
use crate::resource::ResourceMonitor;
use crate::shell::DebugLog;

/// The repaint / sample cadence. ~30 fps keeps animations smooth and the CPU
/// gauge / elapsed timers live without busy-spinning.
const TICK: Duration = Duration::from_millis(33);

/// Configuration for one deck session.
#[derive(Debug, Clone, Default)]
pub struct DeckOptions {
    /// Enable mouse capture (comfy-tabs click/scroll/reorder). Off by default so
    /// native terminal selection keeps working (L-T2).
    pub mouse_capture: bool,
    /// Structured debug log path (`OXAGEN_DEBUG=1`), or `None` for a no-op sink.
    pub debug_log_path: Option<PathBuf>,
    /// An initial code-graph snapshot to seed the Graph tab (the caller, which
    /// owns a `CodeGraph`, queries it and hands it in — the TUI stays decoupled).
    pub initial_graph: Option<GraphSnapshot>,
}

/// Restores the terminal on drop, including during a panic unwind.
///
/// Each terminal state is flagged as it is acquired, and the guard exists
/// BEFORE the first acquisition — so an error partway through `enter` (raw
/// mode on, alternate screen failed) still drops the guard and rolls back
/// exactly the states that were entered, never stranding the user's terminal
/// in raw mode.
struct TerminalGuard {
    raw: bool,
    alt: bool,
    mouse: bool,
}

impl TerminalGuard {
    fn enter(mouse: bool) -> io::Result<Self> {
        let mut guard = Self {
            raw: false,
            alt: false,
            mouse: false,
        };
        let mut out = io::stdout();
        enable_raw_mode()?;
        guard.raw = true;
        execute!(out, EnterAlternateScreen)?;
        guard.alt = true;
        if mouse {
            execute!(out, EnableMouseCapture)?;
            guard.mouse = true;
        }
        Ok(guard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut out = io::stdout();
        if self.mouse {
            let _ = execute!(out, DisableMouseCapture);
        }
        if self.alt {
            let _ = execute!(out, LeaveAlternateScreen);
        }
        if self.raw {
            let _ = disable_raw_mode();
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Run the command deck to completion. [`Inbound`] envelopes stream in over
/// `inbound`; the user's [`WorkspaceInput`]s stream out over `submissions`.
/// Returns when the inbound stream closes or the user quits, having always
/// restored the terminal first.
pub async fn run_deck(
    opts: DeckOptions,
    mut inbound: UnboundedReceiver<Inbound>,
    submissions: UnboundedSender<WorkspaceInput>,
) -> io::Result<()> {
    let debug = DebugLog::new(opts.debug_log_path.clone());
    debug.note("deck session start");

    let _guard = TerminalGuard::enter(opts.mouse_capture)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    let mut model = WorkspaceModel::new();
    model.now_ms = now_ms();
    let mut ui = DeckUi::new(Composer::new());
    ui.graph = opts.initial_graph.clone();
    let mut resources = ResourceMonitor::new();

    // Blocking crossterm reader → async loop, with a shutdown flag.
    let (key_tx, mut key_rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let shutdown = Arc::new(AtomicBool::new(false));
    let reader_shutdown = shutdown.clone();
    let reader = std::thread::spawn(move || {
        while !reader_shutdown.load(Ordering::Relaxed) {
            match event::poll(Duration::from_millis(50)) {
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

    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    'run: loop {
        terminal.draw(|f| render_deck(&model, &mut ui, f))?;

        tokio::select! {
            maybe_inbound = inbound.recv() => {
                match maybe_inbound {
                    Some(ev) => ingest_inbound(&ev, &mut model, &mut ui),
                    // The engine closed the stream — session over.
                    None => break 'run,
                }
            }
            maybe_key = key_rx.recv() => {
                match maybe_key {
                    Some(Event::Key(key)) if key.kind != KeyEventKind::Release => {
                        match handle_deck_key(key, &model, &mut ui) {
                            DeckAction::Quit => {
                                debug.note("user quit");
                                let _ = submissions.send(WorkspaceInput::Quit);
                                break 'run;
                            }
                            DeckAction::Send(input) => {
                                // A queued prompt is reflected locally so it shows
                                // immediately, then forwarded for dispatch — the
                                // input path never blocks on a busy agent.
                                if let WorkspaceInput::Enqueue { text } = &input {
                                    model.queue.enqueue(text.clone(), model.now_ms);
                                }
                                let _ = submissions.send(input);
                            }
                            DeckAction::Handled | DeckAction::Ignored => {}
                        }
                    }
                    // Resize / mouse / paste: the next draw picks them up.
                    Some(_) => {}
                    // Reader thread ended (stdin closed).
                    None => break 'run,
                }
            }
            _ = tick.tick() => {
                // The heartbeat: advance the clock and re-sample resources so
                // gauges, elapsed timers, sparklines, and effects stay live.
                model.now_ms = now_ms();
                resources.sample(&mut model);
            }
        }
    }

    shutdown.store(true, Ordering::Relaxed);
    let _ = reader.join();
    debug.note("deck session end");
    Ok(())
}
