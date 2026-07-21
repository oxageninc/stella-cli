//! Owner-only local-state filesystem primitives.

use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::{Result, StoreError};

pub const WORKSPACE_PRIVATE_DIR: &str = "private";
pub(crate) const WORKSPACE_GENERATED_IGNORE: &[u8] =
    b"*.db\n*.db-wal\n*.db-shm\nreflections.jsonl\nprivate/\n";

#[cfg(unix)]
fn read_committable_file(path: &Path) -> Result<(Vec<u8>, u32)> {
    use std::io::Read as _;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

    let mut options = std::fs::OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let mut file = options.open(path).map_err(|e| {
        StoreError(format!(
            "cannot open committable file {}: {e}",
            path.display()
        ))
    })?;
    let metadata = file
        .metadata()
        .map_err(|e| StoreError(format!("cannot inspect {}: {e}", path.display())))?;
    if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } || metadata.nlink() != 1
    {
        return Err(StoreError(format!(
            "committable file {} must be an owner-controlled single-link regular file",
            path.display()
        )));
    }
    let mode = metadata.permissions().mode() & 0o7777;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|e| StoreError(format!("cannot read {}: {e}", path.display())))?;
    Ok((bytes, mode))
}

#[cfg(unix)]
fn write_committable_atomic(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("tmp.{}.{}", std::process::id(), sequence));
    let mut options = std::fs::OpenOptions::new();
    options
        .write(true)
        .create_new(true)
        .mode(mode)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let mut file = options
        .open(&tmp)
        .map_err(|e| StoreError(format!("cannot create {}: {e}", tmp.display())))?;
    let result = (|| {
        let metadata = file
            .metadata()
            .map_err(|e| StoreError(format!("cannot inspect {}: {e}", tmp.display())))?;
        if !metadata.is_file() {
            return Err(StoreError(format!(
                "temporary committable path {} is not a regular file",
                tmp.display()
            )));
        }
        file.set_permissions(std::fs::Permissions::from_mode(mode))
            .map_err(|e| StoreError(format!("cannot preserve mode on {}: {e}", tmp.display())))?;
        file.write_all(bytes)
            .map_err(|e| StoreError(format!("cannot write {}: {e}", tmp.display())))?;
        file.sync_all()
            .map_err(|e| StoreError(format!("cannot fsync {}: {e}", tmp.display())))?;
        drop(file);
        std::fs::rename(&tmp, path)
            .map_err(|e| StoreError(format!("cannot replace {}: {e}", path.display())))?;
        if let Some(parent) = path.parent() {
            sync_directory(parent)?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(not(unix))]
fn read_committable_file(path: &Path) -> Result<(Vec<u8>, u32)> {
    Err(StoreError(format!(
        "secure committable-file persistence is unsupported on this platform: {}",
        path.display()
    )))
}

#[cfg(not(unix))]
fn write_committable_atomic(path: &Path, _bytes: &[u8], _mode: u32) -> Result<()> {
    Err(StoreError(format!(
        "secure committable-file persistence is unsupported on this platform: {}",
        path.display()
    )))
}

pub(crate) fn ensure_workspace_generated_ignore(dot: &Path) -> Result<()> {
    let path = dot.join(".gitignore");
    let (mut bytes, mode) = match std::fs::symlink_metadata(&path) {
        Ok(_) => read_committable_file(&path)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return write_committable_atomic(&path, WORKSPACE_GENERATED_IGNORE, 0o644);
        }
        Err(error) => {
            return Err(StoreError(format!(
                "cannot inspect generated ignore {}: {error}",
                path.display()
            )));
        }
    };
    if bytes
        .split(|byte| *byte == b'\n')
        .any(|line| line == b"private/")
    {
        return Ok(());
    }
    if !bytes.is_empty() && !bytes.ends_with(b"\n") {
        bytes.push(b'\n');
    }
    bytes.extend_from_slice(b"private/\n");
    write_committable_atomic(&path, &bytes, mode)
}

pub(crate) fn ensure_workspace_state_dir(workspace_root: &Path) -> Result<(PathBuf, bool)> {
    let dir = workspace_root.join(".stella");
    let created = match std::fs::symlink_metadata(&dir) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(StoreError(format!(
                    "workspace state path {} is not a real directory",
                    dir.display()
                )));
            }
            false
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = std::fs::DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder
                .create(&dir)
                .map_err(|e| StoreError(format!("cannot create {}: {e}", dir.display())))?;
            true
        }
        Err(error) => {
            return Err(StoreError(format!(
                "cannot inspect workspace state directory {}: {error}",
                dir.display()
            )));
        }
    };
    Ok((dir, created))
}

fn validate_state_name(name: &str) -> Result<()> {
    let mut components = Path::new(name).components();
    if !matches!(components.next(), Some(std::path::Component::Normal(_)))
        || components.next().is_some()
    {
        return Err(StoreError(format!(
            "private state name must be one filename, got {name:?}"
        )));
    }
    Ok(())
}

fn path_entry_exists(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(StoreError(format!(
            "cannot inspect {}: {error}",
            path.display()
        ))),
    }
}

#[cfg(unix)]
fn validate_safe_legacy(path: &Path, directory: bool) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let metadata = std::fs::symlink_metadata(path).map_err(|e| {
        StoreError(format!(
            "cannot inspect legacy state {}: {e}",
            path.display()
        ))
    })?;
    let expected_type = if directory {
        metadata.is_dir()
    } else {
        metadata.is_file() && metadata.nlink() == 1
    };
    // A regular file inside an owner-only directory is unreachable by other
    // users even if its historical mode is permissive; migration immediately
    // repairs it to 0600 in the new private directory. The parent itself must
    // be owner-only before any legacy path is trusted.
    let owner_only = metadata.uid() == unsafe { libc::geteuid() }
        && (!directory || metadata.permissions().mode() & 0o077 == 0);
    if metadata.file_type().is_symlink() || !expected_type || !owner_only {
        return Err(StoreError(format!(
            "legacy private state {} is not owner-only; left untouched. Restrict its parent to \
             0700 and file to 0600, then retry, or move it aside to create fresh private state",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_safe_legacy(path: &Path, _directory: bool) -> Result<()> {
    Err(StoreError(format!(
        "legacy private state migration is unsupported on this platform; left untouched: {}",
        path.display()
    )))
}

fn migrate_legacy_files(dot: &Path, private: &Path, names: &[String]) -> Result<()> {
    let mut existing: Vec<&String> = Vec::new();
    for name in names {
        if path_entry_exists(&dot.join(name))? {
            existing.push(name);
        }
    }
    if existing.is_empty() {
        return Ok(());
    }
    validate_safe_legacy(dot, true)?;
    for name in &existing {
        let legacy = dot.join(name.as_str());
        validate_safe_legacy(&legacy, false)?;
        if path_entry_exists(&private.join(name.as_str()))? {
            return Err(StoreError(format!(
                "both legacy {} and private {} exist; refusing to choose or overwrite either",
                legacy.display(),
                private.join(name.as_str()).display()
            )));
        }
    }
    ensure_private_dir(private)?;
    for name in existing {
        let legacy = dot.join(name.as_str());
        let target = private.join(name.as_str());
        std::fs::rename(&legacy, &target).map_err(|e| {
            StoreError(format!(
                "cannot migrate legacy private state {} to {}: {e}",
                legacy.display(),
                target.display()
            ))
        })?;
    }
    sync_directory(private)?;
    sync_directory(dot)?;
    Ok(())
}

/// Resolve a workspace-private artifact beneath the owner-only state child,
/// migrating an owner-safe legacy file from the mixed `.stella` directory.
pub fn workspace_private_state_path(workspace_root: &Path, name: &str) -> Result<PathBuf> {
    validate_state_name(name)?;
    let (dot, _) = ensure_workspace_state_dir(workspace_root)?;
    let private = dot.join(WORKSPACE_PRIVATE_DIR);
    ensure_workspace_generated_ignore(&dot)?;
    migrate_legacy_files(&dot, &private, &[name.to_string()])?;
    ensure_private_dir(&private)?;
    Ok(private.join(name))
}

/// Append one line to a workspace-private log through the same no-follow,
/// owner-only file primitive used by session state.
pub fn append_workspace_private_line(
    workspace_root: &Path,
    name: &str,
    line: &str,
) -> Result<PathBuf> {
    use std::io::Write as _;

    let path = workspace_private_state_path(workspace_root, name)?;
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    let mut file = open_private_file(&path, options)?;
    writeln!(file, "{line}")
        .map_err(|e| StoreError(format!("cannot append {}: {e}", path.display())))?;
    Ok(path)
}

/// Resolve a workspace-private SQLite family. A closed, single-file legacy DB
/// can migrate atomically. Live WAL/SHM families fail closed and stay put.
pub fn workspace_private_sqlite_path(workspace_root: &Path, name: &str) -> Result<PathBuf> {
    validate_state_name(name)?;
    let (dot, _) = ensure_workspace_state_dir(workspace_root)?;
    let private = dot.join(WORKSPACE_PRIVATE_DIR);
    ensure_workspace_generated_ignore(&dot)?;
    let sidecars = [format!("{name}-wal"), format!("{name}-shm")];
    let mut existing_sidecars = Vec::new();
    for sidecar in &sidecars {
        if path_entry_exists(&dot.join(sidecar))? {
            existing_sidecars.push(sidecar);
        }
    }
    if !existing_sidecars.is_empty() {
        validate_safe_legacy(&dot, true)?;
        for sidecar in &existing_sidecars {
            validate_safe_legacy(&dot.join(sidecar.as_str()), false)?;
        }
        if path_entry_exists(&dot.join(name))? {
            validate_safe_legacy(&dot.join(name), false)?;
        }
        return Err(StoreError(format!(
            "legacy SQLite state for {} has active WAL/SHM sidecars and was left untouched; \
             close and checkpoint the database, then retry migration into {}",
            dot.join(name).display(),
            private.display()
        )));
    }
    migrate_legacy_files(&dot, &private, &[name.to_string()])?;
    ensure_private_dir(&private)?;
    prepare_private_sqlite_path(&private.join(name))
}

/// Locate an existing workspace-private artifact without creating an empty
/// file. Owner-safe legacy files are migrated; unsafe legacy files error.
pub fn existing_workspace_private_state_path(
    workspace_root: &Path,
    name: &str,
) -> Result<Option<PathBuf>> {
    validate_state_name(name)?;
    let dot = workspace_root.join(".stella");
    if !path_entry_exists(&dot)? {
        return Ok(None);
    }
    ensure_workspace_state_dir(workspace_root)?;
    let private = dot.join(WORKSPACE_PRIVATE_DIR);
    let target = private.join(name);
    if path_entry_exists(&target)? {
        ensure_private_dir(&private)?;
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        drop(open_private_file(&target, options)?);
        return Ok(Some(target));
    }
    let legacy = dot.join(name);
    if !path_entry_exists(&legacy)? {
        return Ok(None);
    }
    workspace_private_state_path(workspace_root, name).map(Some)
}

/// Locate an existing workspace-private SQLite database without creating an
/// empty one. A safe closed legacy database migrates atomically; a legacy
/// database with live sidecars fails closed.
pub fn existing_workspace_private_sqlite_path(
    workspace_root: &Path,
    name: &str,
) -> Result<Option<PathBuf>> {
    validate_state_name(name)?;
    let dot = workspace_root.join(".stella");
    if !path_entry_exists(&dot)? {
        return Ok(None);
    }
    ensure_workspace_state_dir(workspace_root)?;
    let target = dot.join(WORKSPACE_PRIVATE_DIR).join(name);
    let legacy_family_exists = path_entry_exists(&dot.join(name))?
        || path_entry_exists(&dot.join(format!("{name}-wal")))?
        || path_entry_exists(&dot.join(format!("{name}-shm")))?;
    if !path_entry_exists(&target)? && !legacy_family_exists {
        return Ok(None);
    }
    workspace_private_sqlite_path(workspace_root, name).map(Some)
}

/// Create or validate a directory that contains only private local state.
pub(crate) fn ensure_private_dir(dir: &Path) -> Result<()> {
    match std::fs::symlink_metadata(dir) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(StoreError(format!(
                    "private state directory {} is not a real directory",
                    dir.display()
                )));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = std::fs::DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder
                .create(dir)
                .map_err(|e| StoreError(format!("cannot create {}: {e}", dir.display())))?;
        }
        Err(error) => {
            return Err(StoreError(format!(
                "cannot inspect private state directory {}: {error}",
                dir.display()
            )));
        }
    }
    let metadata = std::fs::symlink_metadata(dir)
        .map_err(|e| StoreError(format!("cannot inspect {}: {e}", dir.display())))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(StoreError(format!(
            "private state directory {} changed while opening",
            dir.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(|e| {
            StoreError(format!(
                "cannot restrict private directory {}: {e}",
                dir.display()
            ))
        })?;
    }
    Ok(())
}

/// Open a private regular file without following a terminal symlink.
#[cfg(unix)]
pub(crate) fn open_private_file(
    path: &Path,
    mut options: std::fs::OpenOptions,
) -> Result<std::fs::File> {
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let file = options
        .open(path)
        .map_err(|e| StoreError(format!("cannot open private file {}: {e}", path.display())))?;
    let metadata = file.metadata().map_err(|e| {
        StoreError(format!(
            "cannot inspect private file {}: {e}",
            path.display()
        ))
    })?;
    if !metadata.is_file() {
        return Err(StoreError(format!(
            "private state path {} is not a regular file",
            path.display()
        )));
    }
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.nlink() != 1 {
            return Err(StoreError(format!(
                "private state file {} is not an owner-controlled single-link file (links: {}); \
                 refusing ambiguous ownership",
                path.display(),
                metadata.nlink()
            )));
        }
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|e| {
                StoreError(format!(
                    "cannot restrict private file {}: {e}",
                    path.display()
                ))
            })?;
    }
    Ok(file)
}

/// Platforms without the Unix no-follow/mode-at-create primitives fail closed
/// until an equivalent owner-only implementation exists.
#[cfg(not(unix))]
pub(crate) fn open_private_file(
    path: &Path,
    _options: std::fs::OpenOptions,
) -> Result<std::fs::File> {
    Err(StoreError(format!(
        "secure private file creation is unsupported on this platform: {}",
        path.display()
    )))
}

pub(crate) fn read_private_to_string(path: &Path) -> Result<String> {
    use std::io::Read as _;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    let mut file = open_private_file(path, options)?;
    let mut text = String::new();
    file.read_to_string(&mut text)
        .map_err(|e| StoreError(format!("cannot read {}: {e}", path.display())))?;
    Ok(text)
}

#[cfg(unix)]
fn validate_owner_controlled_parent(path: &Path) -> Result<&Path> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let parent = path
        .parent()
        .ok_or_else(|| StoreError(format!("private file {} has no parent", path.display())))?;
    let metadata = std::fs::symlink_metadata(parent)
        .map_err(|e| StoreError(format!("cannot inspect {}: {e}", parent.display())))?;
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o022 != 0
    {
        return Err(StoreError(format!(
            "sensitive file parent {} must be a real owner-controlled directory with no \
             group/other write permission",
            parent.display()
        )));
    }
    Ok(parent)
}

#[cfg(not(unix))]
fn validate_owner_controlled_parent(path: &Path) -> Result<&Path> {
    Err(StoreError(format!(
        "secure sensitive-file persistence is unsupported on this platform: {}",
        path.display()
    )))
}

/// Atomically replace a sensitive file in an existing owner-controlled
/// directory without following its terminal path.
pub fn write_sensitive_file_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    validate_owner_controlled_parent(path)?;
    write_private_atomic(path, bytes, true)
}

/// Read a sensitive regular file without following its terminal path.
pub fn read_sensitive_file_to_string(path: &Path) -> Result<String> {
    validate_owner_controlled_parent(path)?;
    read_private_to_string(path)
}

/// Atomically write an owner-only session registry or snapshot file.
pub(crate) fn write_private_atomic(path: &Path, bytes: &[u8], sync: bool) -> Result<()> {
    use std::io::Write as _;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    if let Ok(metadata) = std::fs::symlink_metadata(path)
        && (metadata.file_type().is_symlink() || !metadata.is_file())
    {
        return Err(StoreError(format!(
            "private state target {} is not a regular file",
            path.display()
        )));
    }
    let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("tmp.{}.{}", std::process::id(), sequence));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    let mut file = open_private_file(&tmp, options)?;
    let result = (|| {
        file.write_all(bytes)
            .map_err(|e| StoreError(format!("cannot write {}: {e}", tmp.display())))?;
        if sync {
            file.sync_data()
                .map_err(|e| StoreError(format!("cannot fsync {}: {e}", tmp.display())))?;
        }
        drop(file);
        std::fs::rename(&tmp, path)
            .map_err(|e| StoreError(format!("cannot replace {}: {e}", path.display())))?;
        if let Some(parent) = path.parent() {
            sync_directory(parent)?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Pre-create or repair a SQLite main database as an owner-only regular file
/// and return its canonical-parent path.
pub(crate) fn prepare_private_sqlite_path(path: &Path) -> Result<PathBuf> {
    let parent = path
        .parent()
        .ok_or_else(|| StoreError(format!("database path {} has no parent", path.display())))?
        .canonicalize()
        .map_err(|e| StoreError(format!("cannot canonicalize {}: {e}", path.display())))?;
    let name = path
        .file_name()
        .ok_or_else(|| StoreError(format!("database path {} has no filename", path.display())))?;
    let path = parent.join(name);
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true);
    drop(open_private_file(&path, options)?);
    Ok(path)
}

pub(crate) fn sync_directory(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let file = std::fs::File::open(dir).map_err(|e| {
            StoreError(format!(
                "cannot open directory {} for fsync: {e}",
                dir.display()
            ))
        })?;
        file.sync_all()
            .map_err(|e| StoreError(format!("cannot fsync directory {}: {e}", dir.display())))?;
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
    Ok(())
}

pub(crate) fn open_private_sqlite(path: &Path) -> Result<Connection> {
    let path = prepare_private_sqlite_path(path)?;
    let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
        | rusqlite::OpenFlags::SQLITE_OPEN_CREATE
        | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
        | rusqlite::OpenFlags::SQLITE_OPEN_URI
        | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW;
    Connection::open_with_flags(path, flags).map_err(Into::into)
}
