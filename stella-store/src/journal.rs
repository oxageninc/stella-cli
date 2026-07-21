//! Durable per-session state — the crash-safe sidecar that makes every deck
//! session resumable: quit, Ctrl-C, a killed terminal, or a power cut, and
//! `stella resume` (or ⏎ in the SESSIONS overlay) reopens the session exactly
//! where it stood. Nothing the user typed, and nothing a finished turn
//! produced, is ever lost to an interruption.
//!
//! Each session owns one sidecar directory next to its registry record
//! (`data_dir()/sessions/<id>/`, see [`crate::sessions::SessionRegistry`]):
//!
//! - **`journal.jsonl`** — the append-only transcript journal: one
//!   [`JournalRecord`] per line, written live at the deck driver's single
//!   envelope choke point. Replaying it through the deck's pure fold rebuilds
//!   the visible session (transcript, spend, files, traces) byte-for-byte —
//!   the same replay-determinism the fold was designed around.
//! - **`history.json`** — the LLM conversation (`Vec<CompletionMessage>`),
//!   snapshotted atomically at every turn boundary. This is what makes the
//!   conversation *continuable*: the next prompt after a resume picks up from
//!   the last committed turn.
//! - **`queue.json`** — the pending prompt backlog, rewritten atomically on
//!   every queue mutation. A prompt the user queued is durable the moment it
//!   is queued.
//!
//! ## Crash-safety contract
//!
//! The journal is append-only; a torn final line (the write a power cut
//! interrupted) is tolerated and skipped on read. Snapshots are
//! temp+fsync+rename, so a reader sees the old state or the new state, never
//! a torn one. Streamed `Text`/`Reasoning` deltas are coalesced per run and
//! written through the OS page cache; conversation *transitions* (a prompt
//! dispatched, a turn completed or failed, a reset) are fsynced. The worst a
//! power cut can lose is the streamed tail of the turns that were in flight
//! — and those turns' prompts are re-queued on resume (each lane's
//! `PromptStarted` with no settle record after it), so the work itself is
//! never lost.

use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::Path;

use serde::{Deserialize, Serialize};
use stella_protocol::{AgentEvent, CompletionMessage};

use crate::{Result, StoreError};

/// The append-only transcript journal file inside a session's sidecar dir.
pub const JOURNAL_FILE: &str = "journal.jsonl";
/// The LLM conversation snapshot inside a session's sidecar dir.
pub const HISTORY_FILE: &str = "history.json";
/// The pending prompt backlog snapshot inside a session's sidecar dir.
pub const QUEUE_FILE: &str = "queue.json";

/// Flush the coalescing buffer once it holds this many bytes even if the
/// delta run hasn't ended — bounds journal-write latency on very long
/// uninterrupted streams without fsyncing per token.
const DELTA_FLUSH_BYTES: usize = 8 * 1024;

/// One journaled record — the serializable mirror of the deck's fold-relevant
/// inbound envelope (the TUI's `Inbound`). The deck crate deliberately never
/// links this store, so the driver maps one to the other; out-of-band view
/// snapshots (graph, skills, MCP, sessions, notifications…) are regenerated
/// at startup and are *not* journaled — only what the pure fold consumes.
///
/// The wire is additive-only, exactly like `--output-format stream-json`:
/// variants and fields may be added, never renamed, and readers skip lines
/// they cannot parse.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JournalRecord {
    /// An agent joined the workspace (dashboard row + display meta).
    Register {
        agent: String,
        title: String,
        role: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },
    /// One `AgentEvent` belonging to one agent — the transcript stream.
    Event { agent: String, event: AgentEvent },
    /// A supervisor lifecycle transition (snake_case, e.g. `waiting_input`).
    /// `waiting_input` doubles as the settle marker for prompts that were
    /// handled without a model turn (`/help`, `/init`, …).
    Status { agent: String, status: String },
    /// The dispatcher handed a prompt to an agent — the lead, or a
    /// sub-session lane it drained the prompt to. A `PromptStarted` with no
    /// settle record after it on the same agent (`Complete`, a non-retryable
    /// `Error`, or a `waiting_input`/terminal status) marks the turn an
    /// interruption cut short — resume returns its text to the front of the
    /// queue.
    PromptStarted { agent: String, text: String },
    /// `/clear` — the agent's transcript and counters reset to seq 0.
    SessionReset { agent: String },
    /// Staged-pipeline routing toggled; the last value wins on resume.
    Pipeline { on: bool },
}

impl JournalRecord {
    /// Whether landing this record must reach the disk platter (fsync), not
    /// just the OS page cache: the conversation-transition records that a
    /// power cut must never take back. The high-frequency mid-turn stream
    /// (deltas, tool activity, usage ticks) is buffered instead — an
    /// interrupted turn re-runs on resume, so its tail is reproducible.
    fn is_transition(&self) -> bool {
        match self {
            JournalRecord::Register { .. }
            | JournalRecord::Status { .. }
            | JournalRecord::PromptStarted { .. }
            | JournalRecord::SessionReset { .. }
            | JournalRecord::Pipeline { .. } => true,
            JournalRecord::Event { event, .. } => {
                matches!(
                    event,
                    AgentEvent::Complete { .. } | AgentEvent::Error { .. }
                )
            }
        }
    }
}

/// Which streaming-delta kind a pending coalescing run holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeltaKind {
    Text,
    Reasoning,
}

/// An open, coalescing run of streaming deltas not yet written out.
#[derive(Debug)]
struct PendingDeltas {
    agent: String,
    kind: DeltaKind,
    buf: String,
}

/// The append-only journal writer for one session's sidecar dir. One writer
/// per session process (the same single-writer discipline as the registry);
/// readers only ever open the file independently via [`read_journal`].
///
/// Consecutive `Text` (or `Reasoning`) deltas for the same agent are
/// coalesced into one record before hitting the file — the fold concatenates
/// deltas, so replaying the coalesced run is byte-identical to replaying the
/// original stream, at a fraction of the lines and syncs.
#[derive(Debug)]
pub struct SessionJournal {
    file: File,
    pending: Option<PendingDeltas>,
}

impl SessionJournal {
    /// Open (creating the sidecar dir and file as needed) for appending.
    pub fn open(dir: &Path) -> Result<Self> {
        crate::ensure_private_dir(dir)?;
        let path = dir.join(JOURNAL_FILE);
        let mut options = OpenOptions::new();
        // Read is required for the torn-tail recovery probe below; append
        // alone yields a write-only descriptor whose final byte cannot be
        // inspected.
        options.read(true).create(true).append(true);
        let mut file = crate::open_private_file(&path, options)?;
        // Heal a torn tail before appending: without the terminator, the
        // first record written after recovery would fuse into the very line
        // the interruption tore, corrupting BOTH.
        if unterminated_tail(&mut file) {
            file.write_all(b"\n")
                .map_err(|e| StoreError(format!("cannot heal {}: {e}", path.display())))?;
        }
        Ok(Self {
            file,
            pending: None,
        })
    }

    /// Append one record. Streaming deltas coalesce into a pending run;
    /// anything else flushes that run first (order is preserved) and then
    /// lands itself — fsynced when it is a conversation transition.
    pub fn write(&mut self, record: &JournalRecord) -> Result<()> {
        if let JournalRecord::Event { agent, event } = record {
            // Streaming previews are never journaled: the step's `Text` event
            // carries the same text in full (the protocol's authoritative
            // record), so persisting deltas would double every answer on disk
            // — and, arriving one per token, tear the coalescing runs below
            // into per-fragment flushes. Replay renders identically without
            // them.
            if matches!(event, AgentEvent::TextDelta { .. }) {
                return Ok(());
            }
            let (kind, delta) = match event {
                AgentEvent::Text { delta } => (DeltaKind::Text, delta),
                AgentEvent::Reasoning { delta } => (DeltaKind::Reasoning, delta),
                _ => {
                    self.flush_pending()?;
                    return self.write_line(record);
                }
            };
            match &mut self.pending {
                Some(p) if p.agent == *agent && p.kind == kind => p.buf.push_str(delta),
                _ => {
                    self.flush_pending()?;
                    self.pending = Some(PendingDeltas {
                        agent: agent.clone(),
                        kind,
                        buf: delta.clone(),
                    });
                }
            }
            if self
                .pending
                .as_ref()
                .is_some_and(|p| p.buf.len() >= DELTA_FLUSH_BYTES)
            {
                self.flush_pending()?;
            }
            return Ok(());
        }
        self.flush_pending()?;
        self.write_line(record)
    }

    /// Write any pending coalesced run out (page cache, no fsync).
    fn flush_pending(&mut self) -> Result<()> {
        let Some(p) = self.pending.take() else {
            return Ok(());
        };
        let event = match p.kind {
            DeltaKind::Text => AgentEvent::Text { delta: p.buf },
            DeltaKind::Reasoning => AgentEvent::Reasoning { delta: p.buf },
        };
        self.write_line_unsynced(&JournalRecord::Event {
            agent: p.agent,
            event,
        })
    }

    fn write_line(&mut self, record: &JournalRecord) -> Result<()> {
        self.write_line_unsynced(record)?;
        if record.is_transition() {
            self.file
                .sync_data()
                .map_err(|e| StoreError(format!("journal fsync failed: {e}")))?;
        }
        Ok(())
    }

    fn write_line_unsynced(&mut self, record: &JournalRecord) -> Result<()> {
        let mut line = serde_json::to_string(record)
            .map_err(|e| StoreError(format!("cannot serialize journal record: {e}")))?;
        line.push('\n');
        self.file
            .write_all(line.as_bytes())
            .map_err(|e| StoreError(format!("journal write failed: {e}")))
    }

    /// Drain any pending run and fsync — the clean-shutdown / handoff barrier
    /// (called before a session switch and at deck exit).
    pub fn sync(&mut self) -> Result<()> {
        self.flush_pending()?;
        self.file
            .sync_data()
            .map_err(|e| StoreError(format!("journal fsync failed: {e}")))
    }
}

impl Drop for SessionJournal {
    fn drop(&mut self) {
        // Best-effort: never lose a buffered tail to an orderly drop. Errors
        // are unreportable here by construction.
        let _ = self.sync();
    }
}

/// Whether the journal's last byte is NOT a newline — the signature of a
/// write an interruption tore mid-line. Missing/empty/unreadable → false.
fn unterminated_tail(f: &mut File) -> bool {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let Ok(len) = f.seek(SeekFrom::End(0)) else {
        return false;
    };
    if len == 0 {
        return false;
    }
    if f.seek(SeekFrom::End(-1)).is_err() {
        return false;
    }
    let mut last = [0u8; 1];
    f.read_exact(&mut last).is_ok() && last[0] != b'\n'
}

/// Read a session's journal, oldest first. Missing file → empty. A line that
/// does not parse — the torn tail of an interrupted write, or a record from
/// a newer version — is skipped: one bad line never hides the session.
pub fn read_journal(dir: &Path) -> Vec<JournalRecord> {
    let Ok(text) = crate::read_private_to_string(&dir.join(JOURNAL_FILE)) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// Atomically snapshot the LLM conversation (`history.json`).
pub fn write_history(dir: &Path, messages: &[CompletionMessage]) -> Result<()> {
    write_snapshot(dir, HISTORY_FILE, messages)
}

/// Load the LLM conversation snapshot; `None` when absent or unreadable.
pub fn read_history(dir: &Path) -> Option<Vec<CompletionMessage>> {
    let text = crate::read_private_to_string(&dir.join(HISTORY_FILE)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Atomically snapshot the pending prompt backlog (`queue.json`).
pub fn write_queue(dir: &Path, queue: &[String]) -> Result<()> {
    write_snapshot(dir, QUEUE_FILE, queue)
}

/// Load the pending prompt backlog; empty when absent or unreadable.
pub fn read_queue(dir: &Path) -> Vec<String> {
    crate::read_private_to_string(&dir.join(QUEUE_FILE))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Whether a sidecar dir holds anything to restore — the resumability test.
pub fn has_state(dir: &Path) -> bool {
    dir.join(HISTORY_FILE).exists() || dir.join(JOURNAL_FILE).exists()
}

/// temp + fsync + rename: a reader (or a resume after a power cut) sees the
/// old snapshot or the new one, never a torn mix. The registry's plain
/// temp+rename is not enough here — an unfsynced rename can surface an empty
/// file after a crash, and these snapshots ARE the conversation.
fn write_snapshot<T: Serialize + ?Sized>(dir: &Path, name: &str, value: &T) -> Result<()> {
    crate::ensure_private_dir(dir)?;
    let json = serde_json::to_string(value)
        .map_err(|e| StoreError(format!("cannot serialize {name}: {e}")))?;
    let path = dir.join(name);
    crate::write_private_atomic(&path, json.as_bytes(), true)
}

/// Scan a journal for the prompts an interruption cut short: every
/// `PromptStarted` with no settle record after it **on the same agent**, in
/// dispatch order. Turns are serial per agent, but prompts dispatch to
/// parallel lanes (the lead, plus `req:<n>` sub-session workers), so several
/// can be unsettled at once. Settle records are a `Complete`, a
/// **non-retryable** `Error` (a retryable one is a mid-turn warning, not an
/// outcome), a `waiting_input` status (how prompts that never ran a model
/// turn — `/help`, `/init` — settle), or a terminal lane status
/// (`done`/`failed`/`killed` — how sub-session lanes end).
pub fn unsettled_prompts(records: &[JournalRecord]) -> Vec<(String, String)> {
    records
        .iter()
        .enumerate()
        .filter_map(|(i, r)| {
            let JournalRecord::PromptStarted { agent, text } = r else {
                return None;
            };
            let settled = records[i + 1..].iter().any(|r| match r {
                JournalRecord::Event { agent: a, event } if a == agent => matches!(
                    event,
                    AgentEvent::Complete { .. }
                        | AgentEvent::Error {
                            retryable: false,
                            ..
                        }
                ),
                JournalRecord::Status { agent: a, status } if a == agent => {
                    matches!(
                        status.as_str(),
                        "waiting_input" | "done" | "failed" | "killed"
                    )
                }
                _ => false,
            });
            (!settled).then(|| (agent.clone(), text.clone()))
        })
        .collect()
}

/// The last journaled staged-pipeline toggle, if any — resume restores it.
pub fn last_pipeline(records: &[JournalRecord]) -> Option<bool> {
    records.iter().rev().find_map(|r| match r {
        JournalRecord::Pipeline { on } => Some(*on),
        _ => None,
    })
}

/// The last journaled cumulative session spend (`BudgetTick.spent_usd`), if
/// any — resume re-seeds the budget guard so spend stays monotone across
/// interruptions instead of silently resetting to zero.
pub fn last_spent_usd(records: &[JournalRecord]) -> Option<f64> {
    records.iter().rev().find_map(|r| match r {
        JournalRecord::Event {
            event: AgentEvent::BudgetTick { spent_usd, .. },
            ..
        } => Some(*spent_usd),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[cfg(unix)]
    #[test]
    fn journal_sidecar_directory_and_file_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("sidecar");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        drop(SessionJournal::open(&dir).unwrap());
        let mode = |path: &Path| {
            std::fs::symlink_metadata(path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join(JOURNAL_FILE)), 0o600);
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "stella-journal-{tag}-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("t").len()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn text(agent: &str, delta: &str) -> JournalRecord {
        JournalRecord::Event {
            agent: agent.into(),
            event: AgentEvent::Text {
                delta: delta.into(),
            },
        }
    }

    fn complete(agent: &str) -> JournalRecord {
        JournalRecord::Event {
            agent: agent.into(),
            event: AgentEvent::Complete {
                model: "m".into(),
                cost_usd: 0.0,
            },
        }
    }

    fn started(agent: &str, t: &str) -> JournalRecord {
        JournalRecord::PromptStarted {
            agent: agent.into(),
            text: t.into(),
        }
    }

    /// Wire-form equality — the honest comparison for a wire type
    /// (`AgentEvent` deliberately derives no `PartialEq`).
    fn js(records: &[JournalRecord]) -> Vec<String> {
        records
            .iter()
            .map(|r| serde_json::to_string(r).unwrap())
            .collect()
    }

    #[test]
    fn text_delta_previews_never_persist_and_never_tear_a_coalescing_run() {
        let dir = temp_dir("text-delta");
        {
            let mut j = SessionJournal::open(&dir).unwrap();
            j.write(&text("lead", "hel")).unwrap();
            // A preview arriving mid-run must neither land on disk nor flush
            // the pending run — the run below must still coalesce whole.
            j.write(&JournalRecord::Event {
                agent: "lead".into(),
                event: AgentEvent::TextDelta { text: "hel".into() },
            })
            .unwrap();
            j.write(&text("lead", "lo")).unwrap();
            j.write(&complete("lead")).unwrap();
        }
        let back = read_journal(&dir);
        assert_eq!(
            js(&back),
            js(&[text("lead", "hello"), complete("lead")]),
            "one coalesced run, no text_delta lines"
        );
    }

    #[test]
    fn roundtrip_coalesces_delta_runs_preserving_order_and_bytes() {
        let dir = temp_dir("roundtrip");
        {
            let mut j = SessionJournal::open(&dir).unwrap();
            j.write(&JournalRecord::Register {
                agent: "lead".into(),
                title: "t".into(),
                role: "lead".into(),
                model: None,
            })
            .unwrap();
            j.write(&started("lead", "do the thing")).unwrap();
            j.write(&text("lead", "hel")).unwrap();
            j.write(&text("lead", "lo")).unwrap();
            j.write(&JournalRecord::Event {
                agent: "lead".into(),
                event: AgentEvent::Reasoning { delta: "hm".into() },
            })
            .unwrap();
            j.write(&text("lead", " world")).unwrap();
            j.write(&complete("lead")).unwrap();
        }
        let back = read_journal(&dir);
        // Runs coalesce (hel+lo), kind changes split, order is preserved.
        assert_eq!(
            js(&back),
            js(&[
                JournalRecord::Register {
                    agent: "lead".into(),
                    title: "t".into(),
                    role: "lead".into(),
                    model: None,
                },
                started("lead", "do the thing"),
                text("lead", "hello"),
                JournalRecord::Event {
                    agent: "lead".into(),
                    event: AgentEvent::Reasoning { delta: "hm".into() },
                },
                text("lead", " world"),
                complete("lead"),
            ])
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn delta_runs_do_not_merge_across_agents() {
        let dir = temp_dir("agents");
        {
            let mut j = SessionJournal::open(&dir).unwrap();
            j.write(&text("a", "1")).unwrap();
            j.write(&text("b", "2")).unwrap();
            j.write(&text("a", "3")).unwrap();
        }
        assert_eq!(
            js(&read_journal(&dir)),
            js(&[text("a", "1"), text("b", "2"), text("a", "3")])
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn torn_tail_is_skipped_and_the_clean_prefix_survives() {
        let dir = temp_dir("torn");
        {
            let mut j = SessionJournal::open(&dir).unwrap();
            j.write(&started("lead", "p")).unwrap();
            j.write(&complete("lead")).unwrap();
        }
        // Simulate the write a power cut interrupted: a truncated line.
        use std::io::Write as _;
        let mut f = OpenOptions::new()
            .append(true)
            .open(dir.join(JOURNAL_FILE))
            .unwrap();
        f.write_all(b"{\"type\":\"prompt_start").unwrap();
        drop(f);

        assert_eq!(
            js(&read_journal(&dir)),
            js(&[started("lead", "p"), complete("lead")])
        );
        // And appending after recovery keeps working.
        let mut j = SessionJournal::open(&dir).unwrap();
        j.write(&started("lead", "again")).unwrap();
        drop(j);
        assert_eq!(read_journal(&dir).len(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unsettled_prompts_are_the_interrupted_ones_only() {
        let a = "lead".to_string();
        // Settled by Complete.
        let settled = vec![started(&a, "one"), complete(&a)];
        assert_eq!(unsettled_prompts(&settled), vec![]);
        // Settled by a handled command's waiting_input status.
        let handled = vec![
            started(&a, "/help"),
            JournalRecord::Status {
                agent: a.clone(),
                status: "waiting_input".into(),
            },
        ];
        assert_eq!(unsettled_prompts(&handled), vec![]);
        // A retryable error is a warning, not a settle.
        let interrupted = vec![
            started(&a, "one"),
            complete(&a),
            started(&a, "two"),
            JournalRecord::Event {
                agent: a.clone(),
                event: AgentEvent::Error {
                    message: "store write failed".into(),
                    retryable: true,
                },
            },
        ];
        assert_eq!(
            unsettled_prompts(&interrupted),
            vec![(a.clone(), "two".to_string())]
        );
        // A non-retryable error (cancel/abort) settles.
        let aborted = vec![
            started(&a, "two"),
            JournalRecord::Event {
                agent: a.clone(),
                event: AgentEvent::Error {
                    message: "turn stopped by user".into(),
                    retryable: false,
                },
            },
        ];
        assert_eq!(unsettled_prompts(&aborted), vec![]);
        assert_eq!(unsettled_prompts(&[]), vec![]);
    }

    #[test]
    fn unsettled_prompts_track_each_lane_independently() {
        let lead = "lead".to_string();
        let req = "req:1".to_string();
        // A worker's settle must not settle the lead's prompt (and vice
        // versa): the interruption cut BOTH lanes' turns short here, and
        // both come back, in dispatch order.
        let both_cut = vec![
            started(&lead, "refactor the fold"),
            started(&req, "and check the docs"),
            text(&lead, "working…"),
            text(&req, "reading…"),
        ];
        assert_eq!(
            unsettled_prompts(&both_cut),
            vec![
                (lead.clone(), "refactor the fold".to_string()),
                (req.clone(), "and check the docs".to_string()),
            ]
        );
        // A terminal lane status settles that lane only — `killed` (a
        // user-stopped worker) must not resurrect its prompt on resume.
        let worker_done = vec![
            started(&lead, "refactor the fold"),
            started(&req, "and check the docs"),
            JournalRecord::Status {
                agent: req.clone(),
                status: "killed".into(),
            },
        ];
        assert_eq!(
            unsettled_prompts(&worker_done),
            vec![(lead.clone(), "refactor the fold".to_string())]
        );
    }

    #[test]
    fn history_and_queue_snapshots_roundtrip_and_default_empty() {
        let dir = temp_dir("snap");
        assert_eq!(read_history(&dir), None);
        assert!(read_queue(&dir).is_empty());
        assert!(!has_state(&dir));

        let history = vec![
            CompletionMessage::system("sys"),
            CompletionMessage::user("hi"),
            CompletionMessage::assistant("hello"),
        ];
        write_history(&dir, &history).unwrap();
        write_queue(&dir, &["a".into(), "b".into()]).unwrap();
        assert_eq!(read_history(&dir).unwrap(), history);
        assert_eq!(read_queue(&dir), vec!["a".to_string(), "b".to_string()]);
        assert!(has_state(&dir));

        // Overwrite is atomic-replace, not append.
        write_queue(&dir, &[]).unwrap();
        assert!(read_queue(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pipeline_and_spend_scans_take_the_last_value() {
        let records = vec![
            JournalRecord::Pipeline { on: true },
            JournalRecord::Event {
                agent: "lead".into(),
                event: AgentEvent::BudgetTick {
                    spent_usd: 0.5,
                    limit_usd: None,
                    mode: stella_protocol::BudgetMode::Observed,
                },
            },
            JournalRecord::Pipeline { on: false },
            JournalRecord::Event {
                agent: "lead".into(),
                event: AgentEvent::BudgetTick {
                    spent_usd: 1.25,
                    limit_usd: None,
                    mode: stella_protocol::BudgetMode::Observed,
                },
            },
        ];
        assert_eq!(last_pipeline(&records), Some(false));
        assert_eq!(last_spent_usd(&records), Some(1.25));
        assert_eq!(last_pipeline(&[]), None);
        assert_eq!(last_spent_usd(&[]), None);
    }
}
