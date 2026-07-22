//! One-shot credential handoff from a trusted launcher.
//!
//! A benchmark runner must not put a provider key in Stella's environment:
//! model-authored code can otherwise recover it from `env` or from
//! `/proc/$PPID/environ`, even when ordinary child commands remove the
//! variable. The Harbor adapter sends exactly one selected provider key over
//! an inherited anonymous pipe and sets only the non-secret descriptor number
//! and target environment-variable name. Startup consumes and closes that FD
//! before loading project env files, resolving configuration, or creating any
//! runtime/model-controlled subprocess.

use std::io::Read;
use std::sync::OnceLock;

use stella_model::credential::ApiKey;

/// Descriptor number holding the credential bytes. The Harbor adapter uses
/// stdin (fd 0), which Docker Compose connects to an anonymous host pipe.
const HANDOFF_FD_ENV: &str = "STELLA_CREDENTIAL_HANDOFF_FD";
/// Provider credential variable this value stands in for. The *name* is not a
/// secret and lets normal provider routing retain its existing precedence.
const HANDOFF_TARGET_ENV: &str = "STELLA_CREDENTIAL_HANDOFF_TARGET";

const MAX_CREDENTIAL_BYTES: u64 = 64 * 1024;

/// Only credential slots Stella's built-in providers already recognize may be
/// populated through the launcher seam. This prevents a caller from turning
/// the mechanism into an arbitrary inherited-FD-to-environment bridge.
const ALLOWED_TARGETS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "XAI_API_KEY",
    "DEEPSEEK_API_KEY",
    "ZAI_API_KEY",
    "OPENROUTER_API_KEY",
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "VERTEX_ACCESS_TOKEN",
    "LOCAL_API_KEY",
];

struct HandoffCredential {
    target: String,
    value: String,
}

static HANDOFF: OnceLock<HandoffCredential> = OnceLock::new();

/// Consume a configured handoff at single-threaded process startup.
///
/// The descriptor is owned by this function and is closed on every read path.
/// No raw credential is copied into the process environment. Errors contain
/// descriptor/target metadata only, never credential bytes.
pub(crate) fn consume_at_startup() -> Result<(), String> {
    let Some(fd_raw) = std::env::var_os(HANDOFF_FD_ENV) else {
        return Ok(());
    };
    let target = std::env::var(HANDOFF_TARGET_ENV)
        .map_err(|_| format!("{HANDOFF_TARGET_ENV} is required with {HANDOFF_FD_ENV}"))?;
    validate_target(&target)?;
    let fd: i32 = fd_raw
        .to_string_lossy()
        .parse()
        .map_err(|_| format!("{HANDOFF_FD_ENV} must be a non-negative descriptor"))?;
    if fd < 0 {
        return Err(format!(
            "{HANDOFF_FD_ENV} must be a non-negative descriptor"
        ));
    }

    // Disable same-UID memory inspection before the first credential byte is
    // copied into this process. The descriptor is already validated and is
    // owned/closed by `read_and_close_fd`, including if hardening fails.
    let value = read_and_close_fd(fd)?;
    if value.is_empty() {
        return Err(format!("credential handoff for {target} was empty"));
    }

    // SAFETY: main calls this before env-file loading, clap, the Tokio runtime,
    // or any thread creation. Remove the non-secret control metadata as soon
    // as it has served its purpose so children cannot mistake it for a second
    // usable handoff.
    unsafe {
        std::env::remove_var(HANDOFF_FD_ENV);
        std::env::remove_var(HANDOFF_TARGET_ENV);
    }

    HANDOFF
        .set(HandoffCredential { target, value })
        .map_err(|_| "credential handoff was initialized more than once".to_string())
}

/// Once a raw provider credential lives in process memory, same-UID
/// repository code must not be able to ptrace Stella or read
/// `/proc/$PPID/mem`. `PR_SET_DUMPABLE=0` enforces both on Linux and also
/// disables core dumps containing the key. It does not affect ordinary
/// signals, stdout, or mounted-log writes used by Harbor.
#[cfg(target_os = "linux")]
fn harden_process_memory() -> Result<(), String> {
    // SAFETY: `prctl` is called at single-threaded startup with the documented
    // PR_SET_DUMPABLE integer arguments and no pointer parameters.
    let result = unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) };
    if result == 0 {
        Ok(())
    } else {
        Err(format!(
            "could not disable process dumpability for credential handoff: {}",
            std::io::Error::last_os_error()
        ))
    }
}

#[cfg(not(target_os = "linux"))]
fn harden_process_memory() -> Result<(), String> {
    Ok(())
}

/// Resolve an in-memory handoff for one provider credential slot.
pub(crate) fn key_for(target: &str) -> Option<ApiKey> {
    let handoff = HANDOFF.get()?;
    (handoff.target == target).then(|| ApiKey::new(handoff.value.clone()))
}

/// Whether startup consumed a trusted credential handoff.
///
/// Callers use this value-only signal to disable all fallback credential-file
/// discovery. A one-key launcher handoff is authoritative; selecting any other
/// provider must fail rather than silently consulting task-image state.
pub(crate) fn is_present() -> bool {
    HANDOFF.get().is_some()
}

fn validate_target(target: &str) -> Result<(), String> {
    if ALLOWED_TARGETS.contains(&target) {
        Ok(())
    } else {
        Err(format!(
            "credential handoff target `{target}` is not a built-in provider credential"
        ))
    }
}

#[cfg(unix)]
fn read_and_close_fd(fd: i32) -> Result<String, String> {
    read_and_close_fd_with_hardener(fd, harden_process_memory)
}

#[cfg(unix)]
fn read_and_close_fd_with_hardener(
    fd: i32,
    harden: impl FnOnce() -> Result<(), String>,
) -> Result<String, String> {
    use std::os::fd::FromRawFd;

    // SAFETY: the launcher explicitly transfers ownership of this descriptor
    // to Stella. Take ownership before hardening so `File` closes it when this
    // function returns, including the fail-closed hardening error path. Do not
    // read or allocate a credential buffer until memory inspection is off.
    let file = unsafe { std::fs::File::from_raw_fd(fd) };
    harden()?;
    let mut bytes = Vec::new();
    file.take(MAX_CREDENTIAL_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("could not read credential handoff descriptor: {e}"))?;
    if bytes.len() as u64 > MAX_CREDENTIAL_BYTES {
        return Err("credential handoff exceeded the 64 KiB safety limit".to_string());
    }
    // The adapter writes one newline-delimited value to stdin. Strip exactly
    // that framing byte (and an optional CR), never arbitrary whitespace that
    // could hide a malformed credential.
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
    }
    String::from_utf8(bytes).map_err(|_| "credential handoff was not valid UTF-8".to_string())
}

#[cfg(not(unix))]
fn read_and_close_fd(_fd: i32) -> Result<String, String> {
    Err("credential FD handoff is supported only on Unix".to_string())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::{Seek, Write};
    use std::os::fd::{FromRawFd, IntoRawFd};

    #[test]
    fn inherited_pipe_is_consumed_framed_and_closed() {
        let mut fds = [0_i32; 2];
        // SAFETY: valid two-element output buffer.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let mut writer = unsafe { std::fs::File::from_raw_fd(fds[1]) };
        writer.write_all(b"openrouter-test-secret\n").unwrap();
        drop(writer);

        let read_fd = fds[0];
        let value = read_and_close_fd(read_fd).unwrap();
        assert_eq!(value, "openrouter-test-secret");
        // EBADF proves ownership was consumed before any later subprocess.
        assert_eq!(unsafe { libc::fcntl(read_fd, libc::F_GETFD) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EBADF)
        );
    }

    #[test]
    fn target_allowlist_is_provider_specific() {
        assert!(validate_target("OPENROUTER_API_KEY").is_ok());
        assert!(validate_target("GITHUB_TOKEN").is_err());
        assert!(validate_target("LD_PRELOAD").is_err());
    }

    #[test]
    fn oversized_handoff_fails_without_echoing_bytes() {
        let mut fds = [0_i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        // Avoid blocking on pipe capacity: use a temporary anonymous-file FD
        // for the pure size-boundary unit. Production Harbor uses a pipe.
        unsafe {
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
        let file = tempfile::tempfile().unwrap();
        (&file)
            .write_all(&vec![b'x'; MAX_CREDENTIAL_BYTES as usize + 1])
            .unwrap();
        (&file).rewind().unwrap();
        let err = read_and_close_fd(file.into_raw_fd()).unwrap_err();
        assert!(err.contains("64 KiB"));
        assert!(!err.contains("xxx"));
    }

    #[test]
    fn memory_hardening_precedes_read_and_closes_fd_on_failure() {
        let file = tempfile::tempfile().unwrap();
        (&file).write_all(&[0xff, b'\n']).unwrap();
        (&file).rewind().unwrap();
        let fd = file.into_raw_fd();

        let err =
            read_and_close_fd_with_hardener(fd, || Err("synthetic hardening failure".to_string()))
                .unwrap_err();

        // The hardening error wins over the invalid UTF-8 waiting in the FD,
        // proving no credential bytes were read first. EBADF proves ownership
        // was still consumed on that fail-closed path.
        assert_eq!(err, "synthetic hardening failure");
        assert_eq!(unsafe { libc::fcntl(fd, libc::F_GETFD) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EBADF)
        );
    }
}
