//! Host-owned durable idempotency for paid media submissions.
//!
//! This journal belongs in the user's application-data directory, never a
//! workspace. It stores only hashed operation keys, media/provider identity,
//! state, opaque handles, and timestamps. The host identity carries its own
//! trusted expiry, so expired rows can be deleted without making an old retry
//! fresh: the same expired identity is rejected before any insert.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use stella_protocol::MediaKind;

use crate::MediaError;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS media_operations (
    operation_key TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    provider_id TEXT NOT NULL,
    state TEXT NOT NULL,
    handle TEXT,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS media_operations_expires_idx
    ON media_operations(expires_at);
CREATE INDEX IF NOT EXISTS media_operations_handle_idx
    ON media_operations(kind, provider_id, handle);
";

/// Bounded journal policy. `retention_secs` is the maximum accepted lifetime
/// of a host identity; `max_rows` limits live, unexpired operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MediaOperationRetention {
    pub retention_secs: u64,
    pub max_rows: u64,
}

impl Default for MediaOperationRetention {
    fn default() -> Self {
        Self {
            retention_secs: 90 * 24 * 60 * 60,
            max_rows: 100_000,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MediaOperationState {
    Pending,
    ImageCompleted { artifact_id: String },
    VideoSubmitted { provider_job_id: String },
    VideoCompleted { provider_job_id: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MediaOperationClaim {
    New,
    Existing(MediaOperationState),
    Expired,
}

/// Object-safe authority port injected beside the spend gate and host ID.
pub trait MediaOperationJournal: Send + Sync {
    fn claim(
        &self,
        operation_key: &str,
        kind: MediaKind,
        provider_id: &str,
        expires_at: u64,
    ) -> Result<MediaOperationClaim, MediaError>;

    fn finalize(&self, operation_key: &str, state: MediaOperationState) -> Result<(), MediaError>;

    fn complete_video(&self, provider_id: &str, provider_job_id: &str) -> Result<bool, MediaError>;

    fn cancel(&self, operation_key: &str) -> Result<(), MediaError>;
}

/// Indexed SQLite implementation. Every claim/finalize/cleanup is one
/// `BEGIN IMMEDIATE` transaction; SQLite supplies cross-process arbitration
/// and the primary key is the final duplicate-submission backstop.
pub struct SqliteMediaOperationJournal {
    connection: Mutex<Connection>,
    retention: MediaOperationRetention,
}

impl SqliteMediaOperationJournal {
    pub fn open(
        path: impl AsRef<Path>,
        retention: MediaOperationRetention,
    ) -> Result<Self, MediaError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| journal_error(format!("cannot create journal dir: {error}")))?;
        }
        let connection = Connection::open(path)
            .map_err(|error| journal_error(format!("cannot open journal: {error}")))?;
        Self::init(connection, retention)
    }

    pub fn open_in_memory(retention: MediaOperationRetention) -> Result<Self, MediaError> {
        let connection = Connection::open_in_memory()
            .map_err(|error| journal_error(format!("cannot open journal: {error}")))?;
        Self::init(connection, retention)
    }

    fn init(
        connection: Connection,
        retention: MediaOperationRetention,
    ) -> Result<Self, MediaError> {
        if retention.retention_secs == 0 || retention.max_rows == 0 {
            return Err(journal_error("retention and max_rows must be positive"));
        }
        connection
            .busy_timeout(std::time::Duration::from_secs(2))
            .map_err(|error| journal_error(format!("cannot configure journal: {error}")))?;
        connection
            .execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")
            .and_then(|_| connection.execute_batch(SCHEMA))
            .map_err(|error| journal_error(format!("cannot initialize journal: {error}")))?;
        Ok(Self {
            connection: Mutex::new(connection),
            retention,
        })
    }

    pub fn claim_at(
        &self,
        operation_key: &str,
        kind: MediaKind,
        provider_id: &str,
        expires_at: u64,
        now: u64,
    ) -> Result<MediaOperationClaim, MediaError> {
        if operation_key.is_empty() || provider_id.is_empty() {
            return Err(journal_error("operation key and provider are required"));
        }
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| journal_error("journal mutex poisoned"))?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| journal_error(format!("cannot begin claim: {error}")))?;
        expire(&transaction, now)?;
        if expires_at <= now {
            transaction.commit().map_err(sql_error)?;
            return Ok(MediaOperationClaim::Expired);
        }
        if expires_at.saturating_sub(now) > self.retention.retention_secs {
            return Err(journal_error("host operation expiry exceeds retention"));
        }
        let existing = transaction
            .query_row(
                "SELECT kind, provider_id, state, handle, expires_at FROM media_operations WHERE operation_key = ?1",
                [operation_key],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                },
            )
            .optional()
            .map_err(sql_error)?;
        if let Some((stored_kind, stored_provider, state, handle, stored_expiry)) = existing {
            if stored_kind != kind_name(kind)
                || stored_provider != provider_id
                || stored_expiry != sql_integer(expires_at)
            {
                return Err(journal_error(
                    "operation key has conflicting media identity",
                ));
            }
            let state = decode_state(&state, handle)?;
            transaction.commit().map_err(sql_error)?;
            return Ok(MediaOperationClaim::Existing(state));
        }
        let rows: i64 = transaction
            .query_row("SELECT COUNT(*) FROM media_operations", [], |row| {
                row.get(0)
            })
            .map_err(sql_error)?;
        if rows >= sql_integer(self.retention.max_rows) {
            return Err(journal_error("media operation journal capacity reached"));
        }
        transaction
            .execute(
                "INSERT INTO media_operations (operation_key, kind, provider_id, state, handle, created_at, expires_at) VALUES (?1, ?2, ?3, 'pending', NULL, ?4, ?5)",
                params![operation_key, kind_name(kind), provider_id, sql_integer(now), sql_integer(expires_at)],
            )
            .map_err(sql_error)?;
        transaction.commit().map_err(sql_error)?;
        Ok(MediaOperationClaim::New)
    }
}

impl MediaOperationJournal for SqliteMediaOperationJournal {
    fn claim(
        &self,
        operation_key: &str,
        kind: MediaKind,
        provider_id: &str,
        expires_at: u64,
    ) -> Result<MediaOperationClaim, MediaError> {
        self.claim_at(operation_key, kind, provider_id, expires_at, unix_now())
    }

    fn finalize(&self, operation_key: &str, state: MediaOperationState) -> Result<(), MediaError> {
        let (state_name, handle) = encode_state(&state)?;
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| journal_error("journal mutex poisoned"))?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let current = transaction
            .query_row(
                "SELECT kind, state, handle FROM media_operations WHERE operation_key = ?1",
                [operation_key],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(sql_error)?
            .ok_or_else(|| journal_error("cannot finalize unknown or expired operation"))?;
        if current.0 != state_kind(&state) {
            return Err(journal_error("operation state has the wrong media kind"));
        }
        let current = decode_state(&current.1, current.2)?;
        let valid = match (&current, &state) {
            (MediaOperationState::Pending, MediaOperationState::ImageCompleted { .. })
            | (MediaOperationState::Pending, MediaOperationState::VideoSubmitted { .. }) => true,
            (
                MediaOperationState::VideoSubmitted {
                    provider_job_id: left,
                },
                MediaOperationState::VideoCompleted {
                    provider_job_id: right,
                },
            ) => left == right,
            _ => current == state,
        };
        if !valid {
            return Err(journal_error("invalid media operation state transition"));
        }
        transaction
            .execute(
                "UPDATE media_operations SET state = ?2, handle = ?3 WHERE operation_key = ?1",
                params![operation_key, state_name, handle],
            )
            .map_err(sql_error)?;
        transaction.commit().map_err(sql_error)?;
        Ok(())
    }

    fn complete_video(&self, provider_id: &str, provider_job_id: &str) -> Result<bool, MediaError> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| journal_error("journal mutex poisoned"))?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let operation = transaction
            .query_row(
                "SELECT operation_key, state FROM media_operations WHERE kind = 'video' AND provider_id = ?1 AND handle = ?2 AND state IN ('video_submitted', 'video_completed') LIMIT 1",
                params![provider_id, provider_job_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(sql_error)?;
        let Some((operation_key, state)) = operation else {
            transaction.commit().map_err(sql_error)?;
            return Ok(false);
        };
        if state == "video_submitted" {
            transaction
                .execute(
                    "UPDATE media_operations SET state = 'video_completed' WHERE operation_key = ?1",
                    [operation_key],
                )
                .map_err(sql_error)?;
        }
        transaction.commit().map_err(sql_error)?;
        Ok(true)
    }

    fn cancel(&self, operation_key: &str) -> Result<(), MediaError> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| journal_error("journal mutex poisoned"))?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        transaction
            .execute(
                "DELETE FROM media_operations WHERE operation_key = ?1 AND state = 'pending'",
                [operation_key],
            )
            .map_err(sql_error)?;
        transaction.commit().map_err(sql_error)?;
        Ok(())
    }
}

fn expire(transaction: &rusqlite::Transaction<'_>, now: u64) -> Result<(), MediaError> {
    let now = sql_integer(now);
    transaction
        .execute("DELETE FROM media_operations WHERE expires_at <= ?1", [now])
        .map_err(sql_error)?;
    Ok(())
}

fn encode_state(state: &MediaOperationState) -> Result<(&'static str, Option<&str>), MediaError> {
    match state {
        MediaOperationState::Pending => Err(journal_error("pending is created only by claim")),
        MediaOperationState::ImageCompleted { artifact_id } => {
            Ok(("image_completed", Some(artifact_id)))
        }
        MediaOperationState::VideoSubmitted { provider_job_id } => {
            Ok(("video_submitted", Some(provider_job_id)))
        }
        MediaOperationState::VideoCompleted { provider_job_id } => {
            Ok(("video_completed", Some(provider_job_id)))
        }
    }
}

fn decode_state(state: &str, handle: Option<String>) -> Result<MediaOperationState, MediaError> {
    if state == "pending" {
        return match handle {
            None => Ok(MediaOperationState::Pending),
            Some(_) => Err(journal_error("pending operation has a handle")),
        };
    }
    let handle = handle.ok_or_else(|| journal_error("operation handle is missing"))?;
    match state {
        "image_completed" => Ok(MediaOperationState::ImageCompleted {
            artifact_id: handle,
        }),
        "video_submitted" => Ok(MediaOperationState::VideoSubmitted {
            provider_job_id: handle,
        }),
        "video_completed" => Ok(MediaOperationState::VideoCompleted {
            provider_job_id: handle,
        }),
        _ => Err(journal_error("operation state is corrupt")),
    }
}

fn kind_name(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Image => "image",
        MediaKind::Video => "video",
        MediaKind::Svg => "svg",
    }
}

fn state_kind(state: &MediaOperationState) -> &'static str {
    match state {
        MediaOperationState::Pending | MediaOperationState::ImageCompleted { .. } => "image",
        MediaOperationState::VideoSubmitted { .. } | MediaOperationState::VideoCompleted { .. } => {
            "video"
        }
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn sql_integer(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn sql_error(error: rusqlite::Error) -> MediaError {
    journal_error(format!("journal database error: {error}"))
}

fn journal_error(message: impl Into<String>) -> MediaError {
    MediaError::Artifact(message.into())
}
