//! Witness for the credential-resolution resilience contract.
//!
//! `ApiKey::resolve` documents that an interactive caller that cannot actually
//! read stdin should receive a clean [`CredentialError::NotFound`] — "instead of
//! hanging on a read from a stdin that isn't there" (see `resolve`'s doc and the
//! module's "interactive prompt on first use, which never silently fails with an
//! opaque provider error" contract). A non-interactive / unusable stdin must
//! therefore degrade to `NotFound`; it must never surface the raw
//! [`CredentialError::PromptFailed`] that the underlying password reader emits.
//!
//! Inside the `cargo test` harness `std::io::stdin().is_terminal()` reports
//! `true` (libtest drives stdin through a pty/pipe that looks like a terminal),
//! yet the stream is not really readable, so `prompt_password` fails with
//! `ENXIO` ("Device not configured"). The current `resolve` propagates that as
//! `PromptFailed`, breaking the documented "clean NotFound" guarantee. This test
//! pins the intended behavior: it FAILS today (`PromptFailed`) and passes once
//! `resolve` degrades a failed prompt to `NotFound`.
//!
//! Note: this is the resilience/compliance floor (P0 — never hang, never surface
//! an opaque error at a trust boundary) called out by the quality pass. The fix
//! is someone else's job; this test only witnesses the intended contract.

use stella_model::credential::{ApiKey, CredentialError};

/// An env var name unique enough that no other test (or the host environment)
/// will have set it, so `resolve` falls all the way through flag → env → file
/// and reaches the interactive step.
const UNSET_ENV_VAR: &str = "STELLA_WITNESS_PROMPT_DEGRADE_KEY_V1";

#[test]
fn interactive_resolve_with_an_unusable_stdin_degrades_to_not_found() {
    // Make absolutely sure the env var is unset so resolution cannot short-
    // circuit before the prompt step.
    // SAFETY: a uniquely-named env var; the surrounding test owns it.
    unsafe {
        std::env::remove_var(UNSET_ENV_VAR);
    }

    let err = ApiKey::resolve("witness", UNSET_ENV_VAR, None, None, true).unwrap_err();

    // The documented contract: no usable credential → a clean NotFound naming
    // the env var, never the opaque prompt-failure.
    assert!(
        !matches!(err, CredentialError::PromptFailed(_)),
        "an unusable stdin must degrade to NotFound, not surface the raw prompt \
         failure; got {err:?}"
    );
    assert_eq!(
        err,
        CredentialError::NotFound {
            env_var: UNSET_ENV_VAR.to_string()
        },
        "interactive resolution with no usable stdin must yield NotFound"
    );
}
