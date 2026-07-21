"""Host-only credential bundle shared by the secure launcher and adapter.

The benchmark provider key must not exist in Harbor's environment: Docker
Compose performs variable interpolation while building the task environment,
before the installed-agent adapter gets a chance to scrub anything.  The
launcher therefore places provider credentials in one unlinked, owner-only,
seekable file descriptor and execs Harbor with credential-shaped environment
variables removed.  Adapter workers use ``pread`` so concurrent trials never
share or advance a file offset.
"""

from __future__ import annotations

import json
import os
import stat
import tempfile
from collections.abc import Mapping
from typing import Any, BinaryIO

HOST_CREDENTIAL_BUNDLE_FD_ENV = "STELLA_HOST_CREDENTIAL_BUNDLE_FD"
HOST_CREDENTIAL_BUNDLE_SCHEMA = "stella-host-credential-bundle-v1"
HOST_CREDENTIAL_SOURCE = "anonymous-seekable-fd-v1"
ENV_CREDENTIAL_SOURCE = "environment-fallback"
MAX_CREDENTIAL_BUNDLE_BYTES = 256 * 1024

PROVIDER_CREDENTIAL_NAMES = frozenset(
    {
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
    }
)
HOST_ONLY_CONTROL_CREDENTIAL_NAMES = frozenset({"OPENROUTER_MANAGEMENT_API_KEY"})

PROVIDER_CREDENTIAL_NAMES_BY_MODEL_PROVIDER: dict[str, tuple[str, ...]] = {
    "anthropic": ("ANTHROPIC_API_KEY",),
    "openai": ("OPENAI_API_KEY",),
    "xai": ("XAI_API_KEY",),
    "deepseek": ("DEEPSEEK_API_KEY",),
    "zai": ("ZAI_API_KEY",),
    "openrouter": ("OPENROUTER_API_KEY",),
    "gemini": ("GEMINI_API_KEY", "GOOGLE_API_KEY"),
    "google": ("GEMINI_API_KEY", "GOOGLE_API_KEY"),
    "vertex": ("VERTEX_ACCESS_TOKEN",),
}

_AWS_CREDENTIAL_NAMES = frozenset(
    {
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_SESSION_TOKEN",
        "AWS_SECURITY_TOKEN",
        "AWS_WEB_IDENTITY_TOKEN_FILE",
        "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
        "AWS_CONTAINER_CREDENTIALS_FULL_URI",
        "AWS_CONTAINER_AUTHORIZATION_TOKEN",
        "AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE",
        "AWS_SHARED_CREDENTIALS_FILE",
        "AWS_PROFILE",
        "AWS_DEFAULT_PROFILE",
        "GOOGLE_APPLICATION_CREDENTIALS",
        "AZURE_FEDERATED_TOKEN_FILE",
    }
)

# Harbor is a Python process which in turn starts Docker and task-side shells.
# Passing the caller's complete environment would let ambient interpreter,
# dynamic-loader, Git, Compose, or shell startup variables execute mutable code
# after the provider credential FD becomes inheritable.  Keep the host handoff
# deliberately small instead of trying to enumerate every dangerous spelling.
_HARBOR_ENV_ALLOWLIST = frozenset(
    {
        "HOME",
        "LOGNAME",
        "SHELL",
        "TEMP",
        "TERM",
        "TMP",
        "TMPDIR",
        "USER",
        "XDG_CACHE_HOME",
        "XDG_CONFIG_HOME",
        "XDG_RUNTIME_DIR",
        "DOCKER_CERT_PATH",
        "DOCKER_CONFIG",
        "DOCKER_CONTEXT",
        "DOCKER_HOST",
        "DOCKER_TLS_VERIFY",
        "STELLA_BINARY",
        "STELLA_BUDGET",
        "STELLA_DISABLE_REFLECTION",
        "STELLA_SOURCE_COMMIT",
    }
)


def is_credential_env_name(name: str) -> bool:
    """Mirror Stella's subprocess deny-list at the Harbor launch boundary."""
    upper = name.upper()
    return (
        upper in {"API_KEY", "TOKEN", "PASSWORD", "SECRET"}
        or upper.endswith(("_API_KEY", "_TOKEN", "_PASSWORD", "_SECRET"))
        or upper in _AWS_CREDENTIAL_NAMES
    )


def provider_credentials_from_environment(
    environ: Mapping[str, str],
) -> dict[str, str]:
    """Collect only supported provider credentials for the typed bundle."""
    credentials: dict[str, str] = {}
    for name in sorted(PROVIDER_CREDENTIAL_NAMES):
        value = environ.get(name)
        if value:
            credentials[name] = value
    return credentials


def credential_values_from_environment(environ: Mapping[str, str]) -> tuple[str, ...]:
    """Collect every provider or host-only control secret for byte scrubbing."""
    names = PROVIDER_CREDENTIAL_NAMES | HOST_ONLY_CONTROL_CREDENTIAL_NAMES
    return tuple(value for name in sorted(names) if (value := environ.get(name)))


def provider_credential_for_model(
    environ: Mapping[str, str], model: str
) -> dict[str, str]:
    """Select exactly one credential for one literal ``provider/model`` route."""
    provider, separator, model_id = model.partition("/")
    provider = provider.strip().lower()
    if not separator or not provider or not model_id.strip():
        raise RuntimeError("Harbor --model must be a literal provider/model route")
    candidates = PROVIDER_CREDENTIAL_NAMES_BY_MODEL_PROVIDER.get(provider)
    if candidates is None:
        raise RuntimeError(
            "secure launcher does not support the selected model provider"
        )
    selected = {
        name: value
        for name in candidates
        if (value := environ.get(name)) is not None and value != ""
    }
    if len(selected) != 1:
        raise RuntimeError(
            "selected model provider must have exactly one configured credential"
        )
    return selected


def sanitized_harbor_environment(
    environ: Mapping[str, str], bundle_fd: int
) -> dict[str, str]:
    """Allowlist launch variables and expose only the unlinked bundle FD number.

    Scrubbing only credential-shaped names is insufficient: a wrapper or CI
    system can duplicate a provider key under an arbitrary name, and runtime
    variables such as ``PYTHONPATH`` or ``DYLD_INSERT_LIBRARIES`` can execute
    caller-controlled code. Locale variables are the sole prefix-based family;
    every other surviving name is explicit. Values containing a bundled secret
    are removed too, without ever including the secret in diagnostics.
    """
    credential_values = credential_values_from_environment(environ)
    sanitized = {
        str(name): str(value)
        for name, value in environ.items()
        if (str(name) in _HARBOR_ENV_ALLOWLIST or str(name).startswith("LC_"))
        and not is_credential_env_name(str(name))
        and str(name) != HOST_CREDENTIAL_BUNDLE_FD_ENV
        and not any(secret in str(value) for secret in credential_values)
    }
    sanitized[HOST_CREDENTIAL_BUNDLE_FD_ENV] = str(bundle_fd)
    return sanitized


def create_anonymous_credential_bundle(
    credentials: Mapping[str, str],
) -> BinaryIO:
    """Create and populate an unlinked mode-0600 seekable credential FD."""
    unknown = sorted(set(credentials) - PROVIDER_CREDENTIAL_NAMES)
    if unknown:
        raise RuntimeError(
            "credential bundle contains unsupported provider credential names: "
            + ", ".join(unknown)
        )
    if not credentials:
        raise RuntimeError("no supported provider credential is configured")
    normalized: dict[str, str] = {}
    for name, value in credentials.items():
        if not isinstance(value, str) or not value:
            raise RuntimeError(f"provider credential {name} is empty or non-string")
        normalized[name] = value

    payload = json.dumps(
        {
            "schema_version": HOST_CREDENTIAL_BUNDLE_SCHEMA,
            "credentials": normalized,
        },
        sort_keys=True,
        separators=(",", ":"),
        ensure_ascii=False,
    ).encode("utf-8")
    if len(payload) > MAX_CREDENTIAL_BUNDLE_BYTES:
        raise RuntimeError("credential bundle exceeds the 256 KiB safety limit")

    # Ownership intentionally transfers to the caller and must survive exec.
    handle = tempfile.TemporaryFile(mode="w+b")  # noqa: SIM115
    try:
        fd = handle.fileno()
        os.fchmod(fd, 0o600)
        handle.write(payload)
        handle.flush()
        handle.seek(0)
        os.set_inheritable(fd, True)
        return handle
    except BaseException:
        handle.close()
        raise


def _object_without_duplicate_keys(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    value: dict[str, Any] = {}
    for key, item in pairs:
        if key in value:
            raise RuntimeError("credential bundle contains duplicate JSON keys")
        value[key] = item
    return value


def read_anonymous_credential_bundle(fd_text: str) -> dict[str, str]:
    """Validate and pread a launcher-owned credential bundle without seeking."""
    try:
        fd = int(fd_text, 10)
    except (TypeError, ValueError) as exc:
        raise RuntimeError(
            f"{HOST_CREDENTIAL_BUNDLE_FD_ENV} must be a non-negative descriptor"
        ) from exc
    if fd < 0:
        raise RuntimeError(
            f"{HOST_CREDENTIAL_BUNDLE_FD_ENV} must be a non-negative descriptor"
        )

    try:
        info = os.fstat(fd)
    except OSError as exc:
        raise RuntimeError("host credential bundle descriptor is not open") from exc
    if not stat.S_ISREG(info.st_mode) or info.st_nlink != 0:
        raise RuntimeError(
            "host credential bundle must be an unlinked regular temporary file"
        )
    if stat.S_IMODE(info.st_mode) & 0o077:
        raise RuntimeError("host credential bundle must be owner-only mode 0600")
    if not os.get_inheritable(fd):
        raise RuntimeError("host credential bundle descriptor is not inheritable")

    try:
        payload = os.pread(fd, MAX_CREDENTIAL_BUNDLE_BYTES + 1, 0)
    except OSError as exc:
        raise RuntimeError("host credential bundle descriptor is not seekable") from exc
    if len(payload) > MAX_CREDENTIAL_BUNDLE_BYTES:
        raise RuntimeError("credential bundle exceeds the 256 KiB safety limit")
    try:
        decoded = json.loads(
            payload.decode("utf-8"), object_pairs_hook=_object_without_duplicate_keys
        )
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise RuntimeError(
            "host credential bundle is not valid canonical JSON"
        ) from exc
    if not isinstance(decoded, dict) or set(decoded) != {
        "schema_version",
        "credentials",
    }:
        raise RuntimeError("host credential bundle has an invalid top-level shape")
    if decoded["schema_version"] != HOST_CREDENTIAL_BUNDLE_SCHEMA:
        raise RuntimeError("host credential bundle has an unsupported schema version")
    credentials = decoded["credentials"]
    if not isinstance(credentials, dict) or not credentials:
        raise RuntimeError("host credential bundle has no provider credentials")
    unknown = sorted(set(credentials) - PROVIDER_CREDENTIAL_NAMES)
    if unknown:
        raise RuntimeError(
            "host credential bundle contains unsupported provider credential names"
        )
    for name, value in credentials.items():
        if not isinstance(name, str) or not isinstance(value, str) or not value:
            raise RuntimeError("host credential bundle contains an invalid credential")
    return credentials


def read_bundle_from_environment(
    environ: Mapping[str, str],
) -> dict[str, str] | None:
    """Read the host bundle named by ``environ`` or return None for fallback use."""
    fd_text = environ.get(HOST_CREDENTIAL_BUNDLE_FD_ENV)
    if fd_text is None:
        return None
    return read_anonymous_credential_bundle(fd_text)
