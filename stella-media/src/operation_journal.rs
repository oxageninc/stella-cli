//! Host-owned durable idempotency for paid media submissions.
//!
//! This journal belongs in the user's application-data directory, never a
//! workspace. It stores only hashed operation keys, media/provider identity,
//! state, opaque handles, and timestamps. The host identity carries its own
//! trusted expiry, so expired rows can be deleted without making an old retry
//! fresh: the same expired identity is rejected before any insert.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
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
        Self::open_private(path.as_ref(), None, retention)
    }

    /// Open a host-owned journal only when its fully resolved parent remains
    /// outside the model-controlled workspace.
    pub fn open_outside(
        path: impl AsRef<Path>,
        workspace_root: impl AsRef<Path>,
        retention: MediaOperationRetention,
    ) -> Result<Self, MediaError> {
        let workspace_root = workspace_root.as_ref().canonicalize().map_err(|error| {
            journal_error(format!(
                "cannot resolve model-controlled workspace {}: {error}",
                workspace_root.as_ref().display()
            ))
        })?;
        Self::open_private(path.as_ref(), Some(&workspace_root), retention)
    }

    pub fn open_in_memory(retention: MediaOperationRetention) -> Result<Self, MediaError> {
        let connection = Connection::open_in_memory()
            .map_err(|error| journal_error(format!("cannot open journal: {error}")))?;
        Self::init(connection, retention)
    }

    /// One secure open attempt, retried while a concurrent first-open is
    /// initializing the same journal.
    ///
    /// The loser of that race can observe the winner's freshly created WAL
    /// sidecars *between* its own secure preparation and validation — the
    /// exact signature the sidecar-injection defense fails closed on.
    /// Retrying from scratch resolves the ambiguity safely: the re-run
    /// preparation now finds the sidecars up front and subjects them to the
    /// same owner-only validation as any pre-existing pair, so a genuinely
    /// untrusted injection still fails (`security_tests` pin that a single
    /// attempt never tolerates an appearing sidecar).
    fn open_private(
        path: &Path,
        forbidden_root: Option<&Path>,
        retention: MediaOperationRetention,
    ) -> Result<Self, MediaError> {
        let mut attempts = 0;
        loop {
            match Self::open_private_with_hook(path, forbidden_root, retention, |_| {}) {
                Ok(journal) => return Ok(journal),
                Err(error) if attempts < INIT_RETRY_ATTEMPTS && is_sidecar_race(&error) => {
                    attempts += 1;
                    std::thread::sleep(INIT_RETRY_DELAY);
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn open_private_with_hook(
        path: &Path,
        forbidden_root: Option<&Path>,
        retention: MediaOperationRetention,
        after_prepare: impl FnOnce(&Path),
    ) -> Result<Self, MediaError> {
        let prepared = prepare_private_sqlite_path(path, forbidden_root)?;
        after_prepare(&prepared.path);
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW;
        let connection = Connection::open_with_flags(&prepared.path, flags)
            .map_err(|error| journal_error(format!("cannot open journal: {error}")))?;
        prepared.validate_before_init()?;
        let journal = Self::init(connection, retention)?;
        prepared.validate_main_path()?;
        validate_private_sqlite_family(&prepared.path)?;
        drop(prepared);
        Ok(journal)
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
        initialize_journal_database(&connection)
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

/// Runs the idempotent first-open statements, absorbing `SQLITE_BUSY`.
///
/// Converting a fresh rollback-journal database into WAL needs an exclusive
/// lock, and SQLite skips the busy handler on that upgrade to avoid deadlock,
/// so a concurrent first-open gets an immediate `SQLITE_BUSY` that
/// `busy_timeout` never absorbs. Back off until the winner finishes; the
/// loser then observes WAL mode and the existing schema.
fn initialize_journal_database(connection: &Connection) -> Result<(), rusqlite::Error> {
    const BUSY_ATTEMPTS: u32 = 40;
    const BUSY_BACKOFF: std::time::Duration = std::time::Duration::from_millis(50);

    let mut attempts = 0;
    loop {
        let result = connection
            .execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")
            .and_then(|_| connection.execute_batch(SCHEMA));
        match result {
            Err(error)
                if attempts < BUSY_ATTEMPTS
                    && error.sqlite_error_code() == Some(rusqlite::ErrorCode::DatabaseBusy) =>
            {
                attempts += 1;
                std::thread::sleep(BUSY_BACKOFF);
            }
            result => return result,
        }
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

struct PreparedPrivateSqlite {
    path: PathBuf,
    #[cfg(unix)]
    main: PrivateFileGuard,
    #[cfg(unix)]
    sidecars: Vec<PreparedSidecar>,
}

impl PreparedPrivateSqlite {
    #[cfg(unix)]
    fn validate_before_init(&self) -> Result<(), MediaError> {
        self.validate_main_path()?;
        for sidecar in &self.sidecars {
            sidecar.validate()?;
        }
        Ok(())
    }

    #[cfg(unix)]
    fn validate_main_path(&self) -> Result<(), MediaError> {
        self.main.validate_path(&self.path, "journal database")
    }

    #[cfg(not(unix))]
    fn validate_before_init(&self) -> Result<(), MediaError> {
        Ok(())
    }

    #[cfg(not(unix))]
    fn validate_main_path(&self) -> Result<(), MediaError> {
        Ok(())
    }
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PrivateFileIdentity {
    device: u64,
    inode: u64,
    owner: u32,
    links: u64,
    mode: u32,
}

#[cfg(unix)]
struct PrivateFileGuard {
    file: std::fs::File,
    identity: PrivateFileIdentity,
}

#[cfg(unix)]
impl PrivateFileGuard {
    fn validate_path(&self, path: &Path, label: &str) -> Result<(), MediaError> {
        let opened = self.file.metadata().map_err(|error| {
            journal_error(format!(
                "cannot inspect opened {label} {}: {error}",
                path.display()
            ))
        })?;
        let current = std::fs::symlink_metadata(path).map_err(|error| {
            journal_error(format!(
                "cannot re-inspect {label} {}: {error}",
                path.display()
            ))
        })?;
        let opened = private_file_identity(&opened);
        let current = private_file_identity(&current);
        if opened != self.identity || current != self.identity {
            return Err(journal_error(format!(
                "{label} {} changed after its secure open; refusing initialization",
                path.display()
            )));
        }
        Ok(())
    }
}

#[cfg(unix)]
struct PreparedSidecar {
    path: PathBuf,
    guard: Option<PrivateFileGuard>,
}

#[cfg(unix)]
impl PreparedSidecar {
    fn validate(&self) -> Result<(), MediaError> {
        match &self.guard {
            Some(guard) => guard.validate_path(&self.path, "journal SQLite sidecar"),
            None => match std::fs::symlink_metadata(&self.path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Ok(_) => Err(journal_error(format!(
                    "journal SQLite sidecar {} {SIDECAR_APPEARED}; refusing initialization",
                    self.path.display()
                ))),
                Err(error) => Err(journal_error(format!(
                    "cannot re-inspect journal SQLite sidecar {}: {error}",
                    self.path.display()
                ))),
            },
        }
    }
}

#[cfg(unix)]
fn prepare_private_sqlite_path(
    path: &Path,
    forbidden_root: Option<&Path>,
) -> Result<PreparedPrivateSqlite, MediaError> {
    let absolute = std::path::absolute(path).map_err(|error| {
        journal_error(format!(
            "cannot resolve journal path {}: {error}",
            path.display()
        ))
    })?;
    if path
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(journal_error(format!(
            "journal path {} must not contain parent traversal",
            path.display()
        )));
    }
    let name = absolute
        .file_name()
        .ok_or_else(|| journal_error(format!("journal path {} has no filename", path.display())))?
        .to_os_string();
    let parent = absolute
        .parent()
        .ok_or_else(|| journal_error(format!("journal path {} has no parent", path.display())))?;
    let parent = ensure_private_parent(parent, forbidden_root)?;
    let path = parent.join(name);
    let sidecars = prepare_sqlite_sidecars(&path)?;
    let main = precreate_private_file(&path, "journal database")?;
    Ok(PreparedPrivateSqlite {
        path,
        main,
        sidecars,
    })
}

#[cfg(not(unix))]
fn prepare_private_sqlite_path(
    path: &Path,
    _forbidden_root: Option<&Path>,
) -> Result<PreparedPrivateSqlite, MediaError> {
    Err(journal_error(format!(
        "secure host journal persistence is unsupported on this platform: {}",
        path.display()
    )))
}

#[cfg(unix)]
fn ensure_private_parent(
    parent: &Path,
    forbidden_root: Option<&Path>,
) -> Result<PathBuf, MediaError> {
    use std::os::unix::fs::DirBuilderExt;

    let mut cursor = parent;
    let mut missing = Vec::new();
    loop {
        match std::fs::symlink_metadata(cursor) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(journal_error(format!(
                        "journal parent {} is not a real directory",
                        cursor.display()
                    )));
                }
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let name = cursor.file_name().ok_or_else(|| {
                    journal_error(format!(
                        "cannot find an existing ancestor for {}",
                        parent.display()
                    ))
                })?;
                missing.push(name.to_os_string());
                cursor = cursor.parent().ok_or_else(|| {
                    journal_error(format!(
                        "cannot find an existing ancestor for {}",
                        parent.display()
                    ))
                })?;
            }
            Err(error) => {
                return Err(journal_error(format!(
                    "cannot inspect journal parent {}: {error}",
                    cursor.display()
                )));
            }
        }
    }

    let mut resolved = cursor.canonicalize().map_err(|error| {
        journal_error(format!(
            "cannot resolve journal parent {}: {error}",
            cursor.display()
        ))
    })?;
    for name in missing.into_iter().rev() {
        resolved.push(name);
        reject_forbidden_parent(&resolved, forbidden_root)?;
        let mut builder = std::fs::DirBuilder::new();
        builder.mode(0o700);
        match builder.create(&resolved) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(journal_error(format!(
                    "cannot create private journal parent {}: {error}",
                    resolved.display()
                )));
            }
        }
        validate_private_directory(&resolved)?;
    }
    reject_forbidden_parent(&resolved, forbidden_root)?;
    validate_private_directory(&resolved)?;
    Ok(resolved)
}

#[cfg(unix)]
fn reject_forbidden_parent(parent: &Path, forbidden_root: Option<&Path>) -> Result<(), MediaError> {
    if forbidden_root.is_some_and(|root| parent.starts_with(root)) {
        return Err(journal_error(format!(
            "host journal parent {} is inside the model-controlled workspace",
            parent.display()
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn validate_private_directory(path: &Path) -> Result<(), MediaError> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        journal_error(format!(
            "cannot inspect journal parent {}: {error}",
            path.display()
        ))
    })?;
    let mode = metadata.permissions().mode() & 0o777;
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || mode != 0o700
    {
        return Err(journal_error(format!(
            "journal parent {} must be an owner-only 0700 real directory",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn precreate_private_file(path: &Path, label: &str) -> Result<PrivateFileGuard, MediaError> {
    use std::os::unix::fs::OpenOptionsExt;

    let existed = match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(journal_error(format!(
                    "{label} {} must be a regular file",
                    path.display()
                )));
            }
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(journal_error(format!(
                "cannot inspect {label} {}: {error}",
                path.display()
            )));
        }
    };

    let open_existing = || {
        let mut options = std::fs::OpenOptions::new();
        options
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        options.open(path)
    };
    let file = if existed {
        open_existing()
    } else {
        let mut options = std::fs::OpenOptions::new();
        options
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        match options.open(path) {
            Ok(file) => Ok(file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => open_existing(),
            Err(error) => Err(error),
        }
    }
    .map_err(|error| journal_error(format!("cannot open {label} {}: {error}", path.display())))?;
    let identity = validate_opened_private_file(path, &file, label)?;
    Ok(PrivateFileGuard { file, identity })
}

#[cfg(unix)]
fn validate_opened_private_file(
    path: &Path,
    file: &std::fs::File,
    label: &str,
) -> Result<PrivateFileIdentity, MediaError> {
    let opened = file.metadata().map_err(|error| {
        journal_error(format!(
            "cannot inspect opened {label} {}: {error}",
            path.display()
        ))
    })?;
    let current = std::fs::symlink_metadata(path).map_err(|error| {
        journal_error(format!(
            "cannot re-inspect {label} {}: {error}",
            path.display()
        ))
    })?;
    let identity = private_file_identity(&opened);
    if !opened.is_file()
        || identity.owner != unsafe { libc::geteuid() }
        || identity.links != 1
        || identity.mode != 0o600
        || current.file_type().is_symlink()
        || !current.is_file()
        || private_file_identity(&current) != identity
    {
        return Err(journal_error(format!(
            "{label} {} must be an owner-only 0600 single-link regular file",
            path.display()
        )));
    }
    Ok(identity)
}

#[cfg(unix)]
fn private_file_identity(metadata: &std::fs::Metadata) -> PrivateFileIdentity {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    PrivateFileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        owner: metadata.uid(),
        links: metadata.nlink(),
        mode: metadata.permissions().mode() & 0o777,
    }
}

#[cfg(unix)]
fn validate_existing_private_file(path: &Path, label: &str) -> Result<(), MediaError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(journal_error(format!(
                    "{label} {} must be a regular file",
                    path.display()
                )));
            }
            precreate_private_file(path, label).map(drop)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(journal_error(format!(
            "cannot inspect {label} {}: {error}",
            path.display()
        ))),
    }
}

#[cfg(unix)]
fn sqlite_sidecar_path(path: &Path, suffix: &str) -> Result<PathBuf, MediaError> {
    let mut name = path
        .file_name()
        .ok_or_else(|| journal_error(format!("journal path {} has no filename", path.display())))?
        .to_os_string();
    name.push(suffix);
    Ok(path.with_file_name(name))
}

#[cfg(unix)]
fn validate_sqlite_sidecars(path: &Path) -> Result<(), MediaError> {
    for suffix in ["-wal", "-shm"] {
        let sidecar = sqlite_sidecar_path(path, suffix)?;
        validate_existing_private_file(&sidecar, "journal SQLite sidecar")?;
    }
    Ok(())
}

#[cfg(unix)]
fn prepare_sqlite_sidecars(path: &Path) -> Result<Vec<PreparedSidecar>, MediaError> {
    let mut prepared = Vec::with_capacity(2);
    for suffix in ["-wal", "-shm"] {
        let sidecar = sqlite_sidecar_path(path, suffix)?;
        let guard = match std::fs::symlink_metadata(&sidecar) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    return Err(journal_error(format!(
                        "journal SQLite sidecar {} must be a regular file",
                        sidecar.display()
                    )));
                }
                Some(precreate_private_file(&sidecar, "journal SQLite sidecar")?)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(journal_error(format!(
                    "cannot inspect journal SQLite sidecar {}: {error}",
                    sidecar.display()
                )));
            }
        };
        prepared.push(PreparedSidecar {
            path: sidecar,
            guard,
        });
    }
    Ok(prepared)
}

#[cfg(not(unix))]
fn validate_sqlite_sidecars(_path: &Path) -> Result<(), MediaError> {
    Ok(())
}

#[cfg(unix)]
fn validate_private_sqlite_family(path: &Path) -> Result<(), MediaError> {
    validate_existing_private_file(path, "journal database")?;
    validate_sqlite_sidecars(path)
}

#[cfg(not(unix))]
fn validate_private_sqlite_family(_path: &Path) -> Result<(), MediaError> {
    Ok(())
}

#[cfg(all(test, unix))]
mod security_tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn final_path_swap_is_rejected_before_the_external_database_is_mutated() {
        let dir = tempfile::tempdir().unwrap();
        let host = dir.path().join("host-data");
        std::fs::create_dir(&host).unwrap();
        std::fs::set_permissions(&host, std::fs::Permissions::from_mode(0o700)).unwrap();
        let journal_path = host.join("operations.db");
        let external_path = dir.path().join("external.db");
        let external = Connection::open(&external_path).unwrap();
        external
            .execute_batch("CREATE TABLE sentinel (value TEXT NOT NULL);")
            .unwrap();
        drop(external);
        std::fs::set_permissions(&external_path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let result = SqliteMediaOperationJournal::open_private_with_hook(
            &journal_path,
            None,
            MediaOperationRetention {
                retention_secs: 3600,
                max_rows: 100,
            },
            |prepared| {
                std::fs::remove_file(prepared).unwrap();
                std::fs::hard_link(&external_path, prepared).unwrap();
            },
        );

        assert!(result.is_err(), "path substitution must fail closed");
        let external = Connection::open(&external_path).unwrap();
        let journal_tables: i64 = external
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'media_operations'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            journal_tables, 0,
            "an untrusted substitute must not be initialized before rejection"
        );
    }

    #[test]
    fn sidecar_appearance_is_rejected_before_sqlite_initialization() {
        let dir = tempfile::tempdir().unwrap();
        let host = dir.path().join("host-data");
        std::fs::create_dir(&host).unwrap();
        std::fs::set_permissions(&host, std::fs::Permissions::from_mode(0o700)).unwrap();
        let journal_path = host.join("operations.db");
        let injected = host.join("operations.db-wal");

        let result = SqliteMediaOperationJournal::open_private_with_hook(
            &journal_path,
            None,
            MediaOperationRetention {
                retention_secs: 3600,
                max_rows: 100,
            },
            |_| {
                std::fs::write(&injected, b"untrusted-sidecar").unwrap();
                std::fs::set_permissions(&injected, std::fs::Permissions::from_mode(0o600))
                    .unwrap();
            },
        );

        assert!(result.is_err(), "late sidecar must fail closed");
        assert_eq!(std::fs::read(&injected).unwrap(), b"untrusted-sidecar");
        let connection = Connection::open(&journal_path).unwrap();
        let journal_tables: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'media_operations'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(journal_tables, 0);
    }
}
