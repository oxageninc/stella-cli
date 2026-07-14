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

use std::collections::VecDeque;
use std::io;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::composer::{Composer, SlashCommand};
use crate::deck::WorkspaceModel;
use crate::deck_render::render_deck;
use crate::deck_ui::{DeckAction, DeckUi, handle_deck_key, ingest_inbound};
use crate::envelope::{AgentMeta, AgentStatus, Inbound, WorkspaceInput};
use crate::graph::GraphSnapshot;
use crate::resource::ResourceMonitor;
use crate::shell::DebugLog;

/// The repaint / sample cadence. ~30 fps keeps animations smooth and the CPU
/// gauge / elapsed timers live without busy-spinning.
const TICK: Duration = Duration::from_millis(33);

/// The synthetic agent id `!` shell commands run under — they get their own
/// dashboard lane and transcript instead of polluting a real agent's fold.
const SHELL_AGENT: &str = "shell";

/// Cap on captured shell output fed back as an event. Head and tail are both
/// kept (errors live at the tail); the middle is elided.
const SHELL_OUTPUT_CAP: usize = 4000;

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
    /// The slash-command vocabulary for the `/` popup (the caller owns the
    /// real list, exactly like the single-session `RunOptions`).
    pub slash_commands: Vec<SlashCommand>,
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

/// Run one `!` shell command **immediately** on the local event lane.
///
/// The command gets the synthetic [`SHELL_AGENT`] lane: a `Register` (idempotent
/// — re-registering only refreshes the title to the latest command), a
/// `ToolStart` so the invocation is visible the instant it launches, and a
/// `ToolResult` + terminal `Status` when it finishes. stdout and stderr are
/// both captured; a non-zero exit reports as a tool error. The TUI never
/// blocks on the child — it runs on a spawned task and reports back over `tx`.
///
/// `active` counts shell commands currently in flight on the shared
/// [`SHELL_AGENT`] lane. Because immediate `!` commands can overlap (a second
/// one dispatched before the first finishes), only the invocation that drains
/// the count to zero is allowed to park the lane with a terminal `Status` —
/// otherwise an earlier command finishing first would mark the lane
/// Done/Failed while a sibling command is still genuinely running.
fn spawn_shell_command(
    cmd: String,
    tx: UnboundedSender<Inbound>,
    started_ms: u64,
    active: Arc<AtomicUsize>,
) {
    use stella_protocol::{AgentEvent, ToolCall, ToolOutput};

    let call_id = format!("shell-{started_ms}");
    active.fetch_add(1, Ordering::SeqCst);
    let _ = tx.send(Inbound::Register(
        AgentMeta::new(SHELL_AGENT, format!("! {cmd}"), started_ms).with_role("shell"),
    ));
    let _ = tx.send(Inbound::Event {
        agent: SHELL_AGENT.to_string(),
        event: AgentEvent::ToolStart {
            call: ToolCall {
                call_id: call_id.clone(),
                name: "shell".to_string(),
                input: serde_json::json!({ "cmd": cmd }),
            },
        },
    });

    tokio::spawn(async move {
        let started = std::time::Instant::now();
        let spawned = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();

        let (ok, content) = match spawned {
            Ok(mut child) => {
                let stdout = child.stdout.take();
                let stderr = child.stderr.take();
                let mut out_buf = CappedOutput::new();
                let mut err_buf = CappedOutput::new();
                // Read both pipes and wait for exit concurrently — draining
                // stdout/stderr as they arrive (bounded, never fully
                // buffered) so neither pipe can back up and stall the child.
                let (_, _, status) = tokio::join!(
                    read_capped(stdout, &mut out_buf),
                    read_capped(stderr, &mut err_buf),
                    child.wait(),
                );
                let mut text = out_buf.finish();
                if !err_buf.is_empty() {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(&err_buf.finish());
                }
                let success = status.as_ref().map(|s| s.success()).unwrap_or(false);
                if text.trim().is_empty() {
                    let label = status
                        .map(|s| s.to_string())
                        .unwrap_or_else(|e| format!("error: {e}"));
                    text = format!("(no output — exit {label})");
                }
                (success, text)
            }
            Err(e) => (false, format!("failed to spawn `sh -c`: {e}")),
        };
        let output = if ok {
            ToolOutput::Ok { content }
        } else {
            ToolOutput::Error { message: content }
        };
        let _ = tx.send(Inbound::Event {
            agent: SHELL_AGENT.to_string(),
            event: AgentEvent::ToolResult {
                call_id,
                output,
                duration_ms: started.elapsed().as_millis() as u64,
            },
        });
        // Park the lane so it never reads as still-working (a lingering
        // Running shell agent would keep the spinner alive forever) — but
        // only once this was the last command in flight; `fetch_sub` returns
        // the pre-decrement count, so `1` means we just brought it to zero.
        if active.fetch_sub(1, Ordering::SeqCst) == 1 {
            let _ = tx.send(Inbound::Status {
                agent: SHELL_AGENT.to_string(),
                status: if ok {
                    AgentStatus::Done
                } else {
                    AgentStatus::Failed
                },
            });
        }
    });
}

/// Streams a piped child stream into `buf`, chunk by chunk, so output is
/// capped as it arrives rather than fully buffered first.
async fn read_capped<R: tokio::io::AsyncRead + Unpin>(stream: Option<R>, buf: &mut CappedOutput) {
    let Some(mut stream) = stream else { return };
    let mut chunk = [0u8; 8192];
    loop {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.push(&chunk[..n]),
        }
    }
}

/// Bounded middle-out accumulator for shell output: keeps a head window (up
/// to [`SHELL_OUTPUT_CAP`] bytes) and a sliding tail window (the last
/// `SHELL_OUTPUT_CAP / 2` bytes seen), so memory use stays capped regardless
/// of how much a verbose command actually writes — unlike buffering the full
/// stream and truncating only afterward. Errors live at the tail, matching
/// [`spawn_shell_command`]'s stdout-then-stderr ordering.
struct CappedOutput {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    total: usize,
}

impl CappedOutput {
    fn new() -> Self {
        Self {
            head: Vec::new(),
            tail: VecDeque::new(),
            total: 0,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        self.total += chunk.len();
        if self.head.len() < SHELL_OUTPUT_CAP {
            let take = (SHELL_OUTPUT_CAP - self.head.len()).min(chunk.len());
            self.head.extend_from_slice(&chunk[..take]);
        }
        let half = SHELL_OUTPUT_CAP / 2;
        for &b in chunk {
            if self.tail.len() >= half {
                self.tail.pop_front();
            }
            self.tail.push_back(b);
        }
    }

    fn is_empty(&self) -> bool {
        self.total == 0
    }

    /// Renders as a full-buffer-then-truncate implementation would have:
    /// unchanged if it fit, otherwise head + elision marker + tail.
    fn finish(self) -> String {
        if self.total <= SHELL_OUTPUT_CAP {
            return String::from_utf8_lossy(&self.head).into_owned();
        }
        let half = SHELL_OUTPUT_CAP / 2;
        let head = String::from_utf8_lossy(&self.head[..half.min(self.head.len())]).into_owned();
        let tail_bytes: Vec<u8> = self.tail.into_iter().collect();
        let tail = String::from_utf8_lossy(&tail_bytes).into_owned();
        format!("{head}\n…[output truncated]…\n{tail}")
    }
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
    ui.slash_commands = opts.slash_commands.clone();
    let mut resources = ResourceMonitor::new();

    // Synthetic-event lane for `!` shell commands: spawned commands report
    // back here and are folded exactly like engine events. The sender lives
    // for the whole loop, so this arm never closes it.
    let (local_tx, mut local_rx) = tokio::sync::mpsc::unbounded_channel::<Inbound>();
    // Shared in-flight count for overlapping `!` commands (see
    // `spawn_shell_command`) — persists across every dispatch this loop makes.
    let shell_active = Arc::new(AtomicUsize::new(0));

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
                                // Queue edits are reflected locally so they show
                                // immediately, then forwarded for dispatch — the
                                // input path never blocks on a busy agent. (The
                                // queue is the labeled out-of-band fold of the
                                // OUTBOUND stream; this is its one mutation site.)
                                match &input {
                                    WorkspaceInput::Enqueue { text } => {
                                        model.queue.enqueue(text.clone(), model.now_ms);
                                    }
                                    WorkspaceInput::QueueRemove { index } => {
                                        model.queue.remove(*index);
                                    }
                                    WorkspaceInput::QueueClear => model.queue.clear(),
                                    _ => {}
                                }
                                let _ = submissions.send(input);
                            }
                            DeckAction::Shell(cmd) => {
                                // `!` commands run NOW — never queued, never
                                // waiting on the engine. Output returns on the
                                // local lane as ordinary events.
                                debug.note(&format!("shell: {cmd}"));
                                spawn_shell_command(
                                    cmd,
                                    local_tx.clone(),
                                    model.now_ms,
                                    shell_active.clone(),
                                );
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
            maybe_local = local_rx.recv() => {
                // Shell-command lane (see `spawn_shell_command`). `local_tx`
                // outlives the loop, so `None` cannot actually occur.
                if let Some(ev) = maybe_local {
                    ingest_inbound(&ev, &mut model, &mut ui);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capped_output_passes_short_text_through_unchanged() {
        let mut buf = CappedOutput::new();
        buf.push(b"fits");
        assert_eq!(buf.finish(), "fits");
    }

    #[test]
    fn capped_output_keeps_head_and_tail_when_truncated() {
        let mut buf = CappedOutput::new();
        buf.push(b"HEAD");
        buf.push(&vec![b'x'; SHELL_OUTPUT_CAP * 2]);
        buf.push(b"TAIL");
        let out = buf.finish();
        assert!(out.starts_with("HEAD"), "{out}");
        assert!(out.ends_with("TAIL"), "{out}");
        assert!(out.contains("[output truncated]"), "{out}");
    }

    #[test]
    fn capped_output_bounds_memory_regardless_of_input_size() {
        // The whole point of streaming with a bounded accumulator: pushing
        // far more than the cap must not grow internal storage past it,
        // unlike collecting the full output before truncating.
        let mut buf = CappedOutput::new();
        let chunk = vec![b'x'; 8192];
        for _ in 0..64 {
            buf.push(&chunk);
        }
        assert!(buf.head.len() <= SHELL_OUTPUT_CAP);
        assert!(buf.tail.len() <= SHELL_OUTPUT_CAP / 2);
        assert!(buf.finish().contains("[output truncated]"));
    }

    #[test]
    fn shell_commands_report_on_the_local_lane() {
        // The spawner's synchronous part: Register + ToolStart land on the
        // channel immediately, before the child even runs.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let active = Arc::new(AtomicUsize::new(0));
        spawn_shell_command("echo hi".into(), tx, 42, active);
        match rx.try_recv() {
            Ok(Inbound::Register(meta)) => {
                assert_eq!(meta.id, SHELL_AGENT);
                assert!(meta.title.contains("echo hi"));
            }
            other => panic!("expected Register first, got {other:?}"),
        }
        match rx.try_recv() {
            Ok(Inbound::Event { agent, .. }) => assert_eq!(agent, SHELL_AGENT),
            other => panic!("expected ToolStart second, got {other:?}"),
        }
        // The async completion (ToolResult + terminal Status) needs the
        // runtime to run the child; the sync part above is the determinism
        // this test pins, so completion just needs to arrive eventually.
        rt.block_on(async {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await;
        });
    }

    #[test]
    fn overlapping_shell_commands_only_park_the_lane_once_the_last_finishes() {
        // Two `!` commands dispatched before either finishes share the same
        // SHELL_AGENT lane. The fast one (`echo`) must not send a terminal
        // Status while the slow one (`sleep`) is still running — only the
        // last to finish may park the lane.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let active = Arc::new(AtomicUsize::new(0));
        spawn_shell_command("echo fast".into(), tx.clone(), 1, active.clone());
        spawn_shell_command("sleep 0.2 && echo slow".into(), tx, 2, active);

        rt.block_on(async {
            let mut statuses = Vec::new();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while std::time::Instant::now() < deadline {
                match tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await
                {
                    Ok(Some(Inbound::Status { status, .. })) => statuses.push(status),
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(_) => continue,
                }
            }
            assert_eq!(
                statuses.len(),
                1,
                "exactly one terminal Status should be sent for two overlapping commands: {statuses:?}"
            );
        });
    }
}
