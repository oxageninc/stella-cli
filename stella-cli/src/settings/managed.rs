//! Secure administrator-managed settings snapshot.

use std::path::{Path, PathBuf};

use super::Settings;

impl Settings {
    /// Capture secure managed telemetry before project dotenv processing.
    pub(crate) fn load_managed_telemetry_snapshot() -> Result<Option<serde_json::Value>, String> {
        let managed = Self::load_managed_scope(&managed_settings_path())?;
        Ok(managed.enterprise_telemetry)
    }

    pub(super) fn load_managed_scope(path: &Path) -> Result<Self, String> {
        let contents = match read_managed_settings(path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => {
                return Err(format!(
                    "cannot securely read managed settings {}: {error}",
                    path.display()
                ));
            }
        };
        let scope: Settings = serde_json::from_str(&contents)
            .map_err(|error| format!("invalid settings file {}: {error}", path.display()))?;
        for (id, entry) in &scope.providers {
            if let Some(stated) = &entry.id
                && stated != id
            {
                return Err(format!(
                    "settings file {}: providers.{id} declares id `{stated}` — the entry's id must match its key",
                    path.display()
                ));
            }
        }
        Ok(scope)
    }
}

pub(super) fn managed_settings_path() -> PathBuf {
    if let Some(path) = std::env::var_os("STELLA_MANAGED_SETTINGS") {
        return PathBuf::from(path);
    }
    if cfg!(target_os = "macos") {
        PathBuf::from("/Library/Application Support/stella/settings.json")
    } else {
        PathBuf::from("/etc/stella/settings.json")
    }
}

#[cfg(unix)]
fn read_managed_settings(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = file.metadata()?;
    let owner = metadata.uid();
    if !metadata.is_file()
        || metadata.nlink() != 1
        || (owner != unsafe { libc::geteuid() } && owner != 0)
        || metadata.mode() & 0o022 != 0
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "managed settings must be a single-link regular file owned by root or the process user and not group/other writable",
        ));
    }
    let mut contents = String::new();
    file.read_to_string(&mut contents)?;
    Ok(contents)
}

#[cfg(not(unix))]
fn read_managed_settings(_path: &Path) -> std::io::Result<String> {
    Err(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "secure managed settings are unsupported on this platform",
    ))
}
