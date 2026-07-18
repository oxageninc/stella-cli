//! The cross-process **session registry**: every running stella session
//! announces itself as one JSON file under `data_dir()/sessions/`, so any
//! session (or a future `stella sessions` CLI) can render a live "all my
//! stella sessions" view — the deck's SESSIONS overlay reads exactly this.
//!
//! Design: **one file per session, one writer per file.** The owning process
//! is the only writer of its record (atomic temp+rename), so concurrent
//! sessions never contend and there is no lock, daemon, or socket. Readers
//! sweep the directory and are tolerant: an unparsable file is skipped, and
//! a record whose process died mid-flight (pid gone while the status still
//! says in-progress/needs-input) is *presented* as [`SessionStatus::Error`]
//! ("crashed") without rewriting the dead process's file.
//!
//! Lifecycle: the deck driver upserts on session start, on every turn
//! boundary (title/summary/status), and on exit. `Archived` is a user action
//! from the SESSIONS view; archived and other terminal records stay until
//! removed there (or swept by [`SessionRegistry::prune`]).

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::{Result, StoreError};

/// Where a session stands. Serialized in snake_case inside each record file;
/// the SESSIONS view groups by this, in this declaration order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    /// A turn is running (or the session is idle between turns but alive).
    InProgress,
    /// The session is blocked on a human answer (ask-user, scope review).
    NeedsInput,
    /// The user interrupted the work (Ctrl-C mid-turn, queue abandoned).
    Cancelled,
    /// The session ended after finishing its work.
    Complete,
    /// Tucked away by the user from the SESSIONS view; kept until removed.
    Archived,
    /// The session ended on an error — or its process died mid-flight
    /// (derived at read time from a dead pid; see [`SessionRegistry::list`]).
    Error,
}

impl SessionStatus {
    /// Grouping/order for the SESSIONS view: active work first.
    pub const ALL: [SessionStatus; 6] = [
        SessionStatus::InProgress,
        SessionStatus::NeedsInput,
        SessionStatus::Cancelled,
        SessionStatus::Complete,
        SessionStatus::Archived,
        SessionStatus::Error,
    ];

    /// Human group heading.
    pub fn label(&self) -> &'static str {
        match self {
            SessionStatus::InProgress => "In Progress",
            SessionStatus::NeedsInput => "Needs Input",
            SessionStatus::Cancelled => "Cancelled",
            SessionStatus::Complete => "Complete",
            SessionStatus::Archived => "Archived",
            SessionStatus::Error => "Error",
        }
    }

    /// Whether the session still has (or awaits) live work — these states
    /// are pid-checked at read time and downgraded to `Error` if the
    /// process is gone.
    pub fn is_live(&self) -> bool {
        matches!(self, SessionStatus::InProgress | SessionStatus::NeedsInput)
    }
}

/// One session's registry record — everything the SESSIONS view shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecord {
    /// Stable id, minted at session start (`ses-<ms>-<pid>`).
    pub id: String,
    /// The owning process, for read-time liveness checks.
    pub pid: u32,
    /// Absolute workspace path (the human title shows its basename).
    pub workspace: String,
    /// Human-readable title: `<workspace basename>: <first prompt…>`.
    pub title: String,
    /// What work is involved right now — the latest prompt/goal, truncated.
    pub summary: String,
    pub status: SessionStatus,
    pub started_at_ms: u64,
    pub updated_at_ms: u64,
}

impl SessionRecord {
    /// A fresh in-progress record for this process, timestamped now.
    pub fn new(workspace: impl Into<String>, title: impl Into<String>) -> Self {
        let now = now_ms();
        let pid = std::process::id();
        Self {
            id: format!("ses-{now}-{pid}"),
            pid,
            workspace: workspace.into(),
            title: title.into(),
            summary: String::new(),
            status: SessionStatus::InProgress,
            started_at_ms: now,
            updated_at_ms: now,
        }
    }
}

/// The registry directory handle. Cheap to construct; every operation is a
/// direct filesystem op (no cached state to go stale across processes).
#[derive(Debug, Clone)]
pub struct SessionRegistry {
    dir: PathBuf,
}

impl SessionRegistry {
    /// The standard registry at `data_dir()/sessions`.
    pub fn open_default() -> Self {
        Self::open(crate::usage::data_dir().join("sessions"))
    }

    /// A registry rooted at `dir` (tests point this at a temp dir).
    pub fn open(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    fn path_for(&self, id: &str) -> PathBuf {
        // Ids are self-minted (`ses-<ms>-<pid>`), but never trust a name to
        // stay a single path component.
        let safe: String = id
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        self.dir.join(format!("{safe}.json"))
    }

    /// Write (create or replace) `record` atomically, stamping
    /// `updated_at_ms`. Only the owning session should call this for its own
    /// record — except for [`SessionRegistry::set_status`]'s
    /// archive/cleanup writes from the viewer.
    pub fn upsert(&self, record: &SessionRecord) -> Result<()> {
        std::fs::create_dir_all(&self.dir)
            .map_err(|e| StoreError(format!("cannot create {}: {e}", self.dir.display())))?;
        let mut stamped = record.clone();
        stamped.updated_at_ms = now_ms();
        let json = serde_json::to_string_pretty(&stamped)
            .map_err(|e| StoreError(format!("cannot serialize session record: {e}")))?;
        let path = self.path_for(&record.id);
        let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
        std::fs::write(&tmp, json)
            .map_err(|e| StoreError(format!("cannot write {}: {e}", tmp.display())))?;
        std::fs::rename(&tmp, &path)
            .map_err(|e| StoreError(format!("cannot replace {}: {e}", path.display())))?;
        Ok(())
    }

    /// All records, newest-started first, with dead-process downgrade
    /// applied: a live-status record whose pid is gone is shown as `Error`
    /// (the session crashed without writing a terminal status). Unreadable
    /// files are skipped — one corrupt record never hides the rest.
    pub fn list(&self) -> Vec<SessionRecord> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut records: Vec<SessionRecord> = entries
            .filter_map(|entry| {
                let path = entry.ok()?.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    return None;
                }
                let text = std::fs::read_to_string(&path).ok()?;
                let mut record: SessionRecord = serde_json::from_str(&text).ok()?;
                if record.status.is_live() && !pid_alive(record.pid) {
                    record.status = SessionStatus::Error;
                }
                Some(record)
            })
            .collect();
        records.sort_by_key(|r| std::cmp::Reverse(r.started_at_ms));
        records
    }

    /// Read one record (no liveness downgrade — the raw stored state).
    pub fn get(&self, id: &str) -> Option<SessionRecord> {
        let text = std::fs::read_to_string(self.path_for(id)).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Rewrite `id`'s status (the viewer's archive action, and the owner's
    /// terminal transitions). Returns whether the record existed.
    pub fn set_status(&self, id: &str, status: SessionStatus) -> Result<bool> {
        let Some(mut record) = self.get(id) else {
            return Ok(false);
        };
        record.status = status;
        self.upsert(&record)?;
        Ok(true)
    }

    /// Delete `id`'s record; returns whether it existed.
    pub fn remove(&self, id: &str) -> Result<bool> {
        match std::fs::remove_file(self.path_for(id)) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(StoreError(format!("cannot remove session record: {e}"))),
        }
    }

    /// Sweep terminal records older than `max_age_ms` (registry hygiene —
    /// called opportunistically by the deck driver at startup). Live records
    /// are never pruned.
    pub fn prune(&self, max_age_ms: u64) -> Result<usize> {
        let cutoff = now_ms().saturating_sub(max_age_ms);
        let mut removed = 0;
        for record in self.list() {
            if !record.status.is_live() && record.updated_at_ms < cutoff {
                removed += usize::from(self.remove(&record.id)?);
            }
        }
        Ok(removed)
    }
}

/// Whether `pid` is a live process. Unix: `kill(pid, 0)` (EPERM still means
/// alive). Elsewhere: assume alive (no downgrade — better to show a stale
/// in-progress row than to mislabel a live session as crashed).
fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // `pid_t` is signed: a stored pid that doesn't fit (a corrupt
        // record, or a sentinel like `u32::MAX`) must read as dead — an
        // `as` cast would wrap it negative, and `kill(-N, 0)` probes
        // process GROUP N, which can spuriously report alive.
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            return false;
        };
        if pid == 0 {
            return false;
        }
        let rc = unsafe { libc::kill(pid, 0) };
        rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_registry(tag: &str) -> (PathBuf, SessionRegistry) {
        let dir =
            std::env::temp_dir().join(format!("stella-sessions-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        (dir.clone(), SessionRegistry::open(dir))
    }

    #[test]
    fn upsert_list_status_remove_roundtrip() {
        let (dir, reg) = temp_registry("roundtrip");

        let mut rec = SessionRecord::new("/w/space", "space: fix the flaky test");
        rec.summary = "fix the flaky test in stella-tui".into();
        reg.upsert(&rec).unwrap();

        let listed = reg.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, rec.id);
        // Our own pid is alive, so the live status survives the sweep.
        assert_eq!(listed[0].status, SessionStatus::InProgress);

        assert!(reg.set_status(&rec.id, SessionStatus::Archived).unwrap());
        assert_eq!(reg.get(&rec.id).unwrap().status, SessionStatus::Archived);

        assert!(reg.remove(&rec.id).unwrap());
        assert!(!reg.remove(&rec.id).unwrap());
        assert!(reg.list().is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dead_pid_downgrades_live_statuses_to_error_at_read_time() {
        let (dir, reg) = temp_registry("deadpid");

        let mut crashed = SessionRecord::new("/w/a", "a");
        crashed.pid = u32::MAX - 1; // certainly not a live pid
        reg.upsert(&crashed).unwrap();

        let mut done = SessionRecord::new("/w/b", "b");
        done.id = format!("{}-b", done.id); // distinct id even within one ms
        done.pid = u32::MAX - 1;
        done.status = SessionStatus::Complete;
        reg.upsert(&done).unwrap();

        let listed = reg.list();
        let crashed_row = listed.iter().find(|r| r.id == crashed.id).unwrap();
        let done_row = listed.iter().find(|r| r.id == done.id).unwrap();
        // Live status + dead pid → presented as Error…
        assert_eq!(crashed_row.status, SessionStatus::Error);
        // …but the stored file is untouched, and terminal statuses are kept.
        assert_eq!(
            reg.get(&crashed.id).unwrap().status,
            SessionStatus::InProgress
        );
        assert_eq!(done_row.status, SessionStatus::Complete);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_skips_corrupt_files_and_sorts_newest_first() {
        let (dir, reg) = temp_registry("corrupt");

        let mut old = SessionRecord::new("/w/old", "old");
        old.started_at_ms = 1_000;
        reg.upsert(&old).unwrap();
        let mut new = SessionRecord::new("/w/new", "new");
        new.id = format!("{}-b", new.id); // distinct id even within one ms
        new.started_at_ms = 2_000;
        reg.upsert(&new).unwrap();
        std::fs::write(dir.join("garbage.json"), "not json").unwrap();

        let listed = reg.list();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, new.id);
        assert_eq!(listed[1].id, old.id);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_sweeps_only_old_terminal_records() {
        let (dir, reg) = temp_registry("prune");

        let mut live = SessionRecord::new("/w/live", "live");
        reg.upsert(&live).unwrap();
        live = reg.get(&live.id).unwrap();

        let mut done = SessionRecord::new("/w/done", "done");
        done.id = format!("{}-d", done.id);
        done.status = SessionStatus::Complete;
        reg.upsert(&done).unwrap();
        // Backdate the terminal record past any cutoff (bypass upsert's
        // restamping by rewriting the file directly).
        let mut old = reg.get(&done.id).unwrap();
        old.updated_at_ms = 1;
        std::fs::write(
            dir.join(format!("{}.json", old.id)),
            serde_json::to_string(&old).unwrap(),
        )
        .unwrap();

        let removed = reg.prune(60_000).unwrap();
        assert_eq!(removed, 1);
        assert!(reg.get(&done.id).is_none());
        assert_eq!(reg.get(&live.id).unwrap().id, live.id);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
