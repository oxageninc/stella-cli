//! **Persist-until-read notifications.** A notification is a message that
//! must survive until the user has actually seen it — an OAuth login
//! finishing, another session blocking on input, a background run erroring —
//! not a transient toast that vanishes with the frame.
//!
//! Storage mirrors the session registry: **one JSON file per notification**
//! under `data_dir()/notifications/`, written atomically. Producers in
//! different processes never contend (each mints its own file), and marking
//! read rewrites only that one file. The deck shows an unread badge and an
//! inbox overlay; a notification leaves the unread set only when the user
//! marks it read there. Read notifications are swept by
//! [`NotificationStore::prune`].

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::sessions::now_ms;
use crate::{Result, StoreError};

/// One persistent message. `read` is the whole lifecycle: false → shown in
/// the badge/inbox, true → kept only until pruned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notification {
    /// Unique id (`ntf-<ms>-<pid>-<seq>`), minted by [`Notification::new`].
    pub id: String,
    pub created_at_ms: u64,
    /// One-line headline (the badge/inbox row).
    pub title: String,
    /// The message that must persist until read.
    pub body: String,
    /// Where it came from — a session id from the registry, a server name,
    /// etc. Free-form; the inbox shows it dimmed.
    #[serde(default)]
    pub source: String,
    /// The session registry id ([`crate::SessionRecord::id`]) this
    /// notification concerns, when known — the structured link the inbox
    /// uses to jump to the session (unlike `source`, which is free-form).
    /// `default` so notification files written before the field existed
    /// still parse.
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub read: bool,
}

impl Notification {
    pub fn new(
        title: impl Into<String>,
        body: impl Into<String>,
        source: impl Into<String>,
    ) -> Self {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let now = now_ms();
        Self {
            id: format!(
                "ntf-{now}-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ),
            created_at_ms: now,
            title: title.into(),
            body: body.into(),
            source: source.into(),
            session_id: None,
            read: false,
        }
    }

    /// The same notification linked to a session (builder-style, so
    /// producers that know their session id can chain it onto
    /// [`Notification::new`]).
    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }
}

/// The notification directory handle; every operation is a direct
/// filesystem op, so any number of sessions can produce and one can read.
#[derive(Debug, Clone)]
pub struct NotificationStore {
    dir: PathBuf,
}

impl NotificationStore {
    /// The standard store at `data_dir()/notifications`.
    pub fn open_default() -> Self {
        Self::open(crate::usage::data_dir().join("notifications"))
    }

    /// A store rooted at `dir` (tests point this at a temp dir).
    pub fn open(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    fn path_for(&self, id: &str) -> PathBuf {
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

    /// Persist `notification` (its own file — concurrent producers never
    /// clobber each other).
    pub fn push(&self, notification: &Notification) -> Result<()> {
        crate::ensure_private_dir(&self.dir)?;
        let json = serde_json::to_string_pretty(notification)
            .map_err(|e| StoreError(format!("cannot serialize notification: {e}")))?;
        let path = self.path_for(&notification.id);
        crate::write_private_atomic(&path, json.as_bytes(), false)
    }

    /// All notifications, newest first. Unreadable files are skipped.
    pub fn list(&self) -> Vec<Notification> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut items: Vec<Notification> = entries
            .filter_map(|entry| {
                let path = entry.ok()?.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    return None;
                }
                serde_json::from_str(&crate::read_private_to_string(&path).ok()?).ok()
            })
            .collect();
        items.sort_by_key(|n| std::cmp::Reverse(n.created_at_ms));
        items
    }

    /// How many are still unread (the badge count).
    pub fn unread_count(&self) -> usize {
        self.list().iter().filter(|n| !n.read).count()
    }

    /// Mark one read; returns whether it existed.
    pub fn mark_read(&self, id: &str) -> Result<bool> {
        let path = self.path_for(id);
        let Ok(text) = crate::read_private_to_string(&path) else {
            return Ok(false);
        };
        let Ok(mut notification) = serde_json::from_str::<Notification>(&text) else {
            return Ok(false);
        };
        if !notification.read {
            notification.read = true;
            self.push(&notification)?;
        }
        Ok(true)
    }

    /// Mark everything read (the inbox's `R`).
    pub fn mark_all_read(&self) -> Result<usize> {
        let mut marked = 0;
        for notification in self.list() {
            if !notification.read {
                marked += usize::from(self.mark_read(&notification.id)?);
            }
        }
        Ok(marked)
    }

    /// Sweep **read** notifications older than `max_age_ms`. Unread ones are
    /// never pruned — "persists until read" is the contract.
    pub fn prune(&self, max_age_ms: u64) -> Result<usize> {
        let cutoff = now_ms().saturating_sub(max_age_ms);
        let mut removed = 0;
        for notification in self.list() {
            if notification.read && notification.created_at_ms < cutoff {
                match std::fs::remove_file(self.path_for(&notification.id)) {
                    Ok(()) => removed += 1,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(StoreError(format!("cannot prune notification: {e}"))),
                }
            }
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(tag: &str) -> (PathBuf, NotificationStore) {
        let dir = std::env::temp_dir().join(format!("stella-notify-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        (dir.clone(), NotificationStore::open(dir))
    }

    #[test]
    fn push_list_and_mark_read_lifecycle() {
        let (dir, store) = temp_store("lifecycle");

        let a = Notification::new("login ok", "github: OAuth login completed", "mcp");
        let b = Notification::new("needs input", "session ses-1 is waiting", "ses-1");
        store.push(&a).unwrap();
        store.push(&b).unwrap();

        assert_eq!(store.unread_count(), 2);
        let listed = store.list();
        assert_eq!(listed.len(), 2);

        assert!(store.mark_read(&a.id).unwrap());
        assert!(!store.mark_read("ntf-nope").unwrap());
        assert_eq!(store.unread_count(), 1);
        // The read flag persisted to disk.
        assert!(store.list().iter().any(|n| n.id == a.id && n.read));

        assert_eq!(store.mark_all_read().unwrap(), 1);
        assert_eq!(store.unread_count(), 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn notification_user_data_is_owner_only_and_symlinks_are_rejected() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("notifications");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        let store = NotificationStore::open(&dir);
        let first = Notification::new("private", "sensitive body", "session");
        store.push(&first).unwrap();
        let mode = |path: &std::path::Path| {
            std::fs::symlink_metadata(path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&store.path_for(&first.id)), 0o600);

        let second = Notification::new("other", "private", "session");
        let target = tmp.path().join("outside.json");
        std::fs::write(&target, "outside").unwrap();
        symlink(&target, store.path_for(&second.id)).unwrap();
        assert!(store.push(&second).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "outside");
    }

    #[test]
    fn session_id_roundtrips_and_legacy_files_still_parse() {
        let (dir, store) = temp_store("session-id");

        let linked = Notification::new("pr opened", "fleet: PR #7 opened", "fleet")
            .with_session_id("ses-42-7");
        store.push(&linked).unwrap();
        // A notification file written before session_id existed — the field
        // is simply absent from its JSON.
        let legacy = r#"{
          "id": "ntf-legacy-1",
          "created_at_ms": 5,
          "title": "old",
          "body": "written before session_id existed",
          "source": "mcp",
          "read": false
        }"#;
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ntf-legacy-1.json"), legacy).unwrap();

        let listed = store.list();
        assert_eq!(listed.len(), 2);
        let new = listed.iter().find(|n| n.id == linked.id).unwrap();
        assert_eq!(new.session_id.as_deref(), Some("ses-42-7"));
        let old = listed.iter().find(|n| n.id == "ntf-legacy-1").unwrap();
        assert_eq!(old.session_id, None, "missing field must default to None");
        // Persist-until-read is untouched: the legacy file is still unread.
        assert_eq!(store.unread_count(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_never_touches_unread() {
        let (dir, store) = temp_store("prune");

        let mut old_read = Notification::new("done", "old + read", "");
        old_read.created_at_ms = 1;
        old_read.read = true;
        store.push(&old_read).unwrap();

        let mut old_unread = Notification::new("still waiting", "old + UNREAD", "");
        old_unread.id = format!("{}-u", old_unread.id);
        old_unread.created_at_ms = 1;
        store.push(&old_unread).unwrap();

        assert_eq!(store.prune(1_000).unwrap(), 1);
        let left = store.list();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].id, old_unread.id);
        assert!(!left[0].read, "persist-until-read must survive pruning");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
