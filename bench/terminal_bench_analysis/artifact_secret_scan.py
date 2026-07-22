#!/usr/bin/env python3
"""Fail-closed credential scan for benchmark publication artifacts.

The scanner never prints a credential or matching byte context. It checks
exact values from sensitive environment variables (including common encoded
forms) and high-confidence provider token formats.
"""

from __future__ import annotations

import argparse
import base64
import bz2
import gzip
import hashlib
import io
import json
import lzma
import os
import re
import stat
import sys
import tarfile
import zipfile
from collections.abc import Iterable, Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Any
from urllib.parse import quote

_MAX_ARCHIVE_BYTES = 128 * 1024 * 1024
_MAX_ARCHIVE_TOTAL_BYTES = 512 * 1024 * 1024
_MAX_ARCHIVE_MEMBERS = 10_000
_MAX_ARCHIVE_DEPTH = 4
_UNSUPPORTED_ARCHIVE_SUFFIXES = (".7z", ".rar", ".zst", ".zstd")
_UNSUPPORTED_ARCHIVE_MAGIC = (b"7z\xbc\xaf'\x1c", b"Rar!\x1a\x07", b"\x28\xb5\x2f\xfd")

DEFAULT_SECRET_ENV_NAMES = (
    "OPENROUTER_API_KEY",
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "GOOGLE_API_KEY",
    "GEMINI_API_KEY",
    "AZURE_OPENAI_API_KEY",
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "GITHUB_TOKEN",
    "GH_TOKEN",
    "GITLAB_TOKEN",
)

SENSITIVE_ENV_NAME = re.compile(
    r"(?:^|_)(?:API_KEY|TOKEN|SECRET|PASSWORD|CREDENTIALS?|PRIVATE_KEY)$",
    re.IGNORECASE,
)

# Provider-specific prefixes and AWS access-key structure keep this check
# high-confidence. Generic Bearer matching creates excessive false positives.
TOKEN_PATTERNS: tuple[tuple[str, re.Pattern[bytes]], ...] = (
    ("openrouter-token", re.compile(rb"sk-or-v1-[A-Za-z0-9_-]{20,}")),
    (
        "openai-token",
        re.compile(rb"sk-(?!(?:or-v1|ant)-)(?:proj-|svcacct-)?[A-Za-z0-9_-]{24,}"),
    ),
    ("anthropic-token", re.compile(rb"sk-ant-[A-Za-z0-9_-]{20,}")),
    ("github-token", re.compile(rb"gh[pousr]_[A-Za-z0-9]{30,}")),
    ("github-fine-grained-token", re.compile(rb"github_pat_[A-Za-z0-9_]{30,}")),
    ("aws-access-key", re.compile(rb"(?:AKIA|ASIA)[A-Z0-9]{16}")),
)


@dataclass(frozen=True)
class Needle:
    label: str
    value: bytes


@dataclass(frozen=True)
class Finding:
    path: str
    kind: str


def _encoded_variants(value: str) -> Iterable[tuple[str, bytes]]:
    raw = value.encode("utf-8")
    yield "raw", raw
    yield "reversed", raw[::-1]
    yield "json", json.dumps(value, ensure_ascii=False)[1:-1].encode("utf-8")
    yield "json-ascii", json.dumps(value, ensure_ascii=True)[1:-1].encode("ascii")
    yield "json-unicode", "".join(f"\\u{byte:04x}" for byte in raw).encode("ascii")
    yield "url", quote(value, safe="").encode("ascii")
    encoded_base64 = base64.b64encode(raw)
    encoded_base64url = base64.urlsafe_b64encode(raw)
    encoded_base32 = base64.b32encode(raw)
    yield "base64", encoded_base64
    yield "base64-unpadded", encoded_base64.rstrip(b"=")
    yield "base64url", encoded_base64url
    yield "base64url-unpadded", encoded_base64url.rstrip(b"=")
    yield "base32", encoded_base32
    yield "base32-unpadded", encoded_base32.rstrip(b"=")
    yield "base32-lower", encoded_base32.lower()
    yield "base32-lower-unpadded", encoded_base32.lower().rstrip(b"=")
    yield "hex", raw.hex().encode("ascii")
    yield "hex-upper", raw.hex().upper().encode("ascii")


def environment_needles(
    environ: Mapping[str, str], explicit_names: Sequence[str] = ()
) -> tuple[Needle, ...]:
    names = set(DEFAULT_SECRET_ENV_NAMES)
    names.update(explicit_names)
    names.update(name for name in environ if SENSITIVE_ENV_NAME.search(name))

    needles: dict[tuple[str, bytes], Needle] = {}
    for name in sorted(names):
        value = environ.get(name)
        if value is None or len(value.encode("utf-8")) < 8:
            continue
        for variant, encoded in _encoded_variants(value):
            key = (name, encoded)
            needles[key] = Needle(f"env:{name}:{variant}", encoded)
    return tuple(needles.values())


def _scan_bytes(data: bytes, needles: Sequence[Needle]) -> set[str]:
    hits = {needle.label for needle in needles if needle.value in data}
    hits.update(label for label, pattern in TOKEN_PATTERNS if pattern.search(data))
    return hits


def _safe_path(path: str, needles: Sequence[Needle]) -> str:
    """Return a path safe to print even when its name embeds a credential."""
    if not _scan_bytes(path.encode("utf-8", errors="surrogateescape"), needles):
        return path
    digest = hashlib.sha256(path.encode("utf-8", errors="surrogateescape")).hexdigest()
    return f"<redacted-path:{digest[:12]}>"


def _iter_tree(root: Path) -> Iterable[tuple[Path, str]]:
    """Walk without suppressing unreadable subtrees or special file types."""
    if not os.path.lexists(root):
        raise FileNotFoundError(root)

    root_mode = root.lstat().st_mode
    if stat.S_ISLNK(root_mode):
        yield root, "symlink"
        return
    if stat.S_ISREG(root_mode):
        yield root, "file"
        return
    if not stat.S_ISDIR(root_mode):
        yield root, "special"
        return

    def visit(directory: Path) -> Iterable[tuple[Path, str]]:
        # Materialize entries while the scandir context is open so any read or
        # stat failure propagates to the caller and blocks publication.
        with os.scandir(directory) as iterator:
            entries = sorted(iterator, key=lambda entry: entry.name)
        for entry in entries:
            path = Path(entry.path)
            if entry.is_symlink():
                yield path, "symlink"
            elif entry.is_dir(follow_symlinks=False):
                yield path, "directory"
                yield from visit(path)
            elif entry.is_file(follow_symlinks=False):
                yield path, "file"
            else:
                yield path, "special"

    yield from visit(root)


def _read_limited(handle: Any, limit: int = _MAX_ARCHIVE_BYTES) -> bytes | None:
    data = handle.read(limit + 1)
    return None if len(data) > limit else data


def _looks_like_archive(data: bytes, suffix: str = "") -> bool:
    return (
        data.startswith((b"PK\x03\x04", b"\x1f\x8b", b"BZh", b"\xfd7zXZ\x00"))
        or (len(data) > 262 and data[257:262] == b"ustar")
        or suffix.lower() in {".zip", ".tar", ".gz", ".tgz", ".bz2", ".xz", ".lzma"}
    )


def _scan_archive_blob(
    data: bytes,
    display_path: str,
    needles: Sequence[Needle],
    *,
    depth: int = 0,
) -> list[Finding]:
    """Inspect supported compressed/archive bytes without extracting to disk."""
    findings: list[Finding] = []
    if not _looks_like_archive(data):
        return findings
    if depth >= _MAX_ARCHIVE_DEPTH:
        return [Finding(display_path, "archive-depth-limit")]

    def scan_member(member_name: str, member_data: bytes | None) -> None:
        raw_member_display = f"{display_path}!{member_name}"
        member_path_hits = _scan_bytes(
            raw_member_display.encode("utf-8", errors="surrogateescape"), needles
        )
        member_display = _safe_path(raw_member_display, needles)
        findings.extend(
            Finding(member_display, f"path:{kind}") for kind in sorted(member_path_hits)
        )
        if member_data is None:
            findings.append(Finding(member_display, "archive-entry-size-limit"))
            return
        findings.extend(
            Finding(member_display, kind)
            for kind in sorted(_scan_bytes(member_data, needles))
        )
        findings.extend(
            _scan_archive_blob(
                member_data,
                member_display,
                needles,
                depth=depth + 1,
            )
        )

    try:
        if data.startswith(b"PK\x03\x04"):
            with zipfile.ZipFile(io.BytesIO(data)) as archive:
                members = archive.infolist()
                if len(members) > _MAX_ARCHIVE_MEMBERS:
                    return [Finding(display_path, "archive-member-count-limit")]
                if (
                    sum(member.file_size for member in members)
                    > _MAX_ARCHIVE_TOTAL_BYTES
                ):
                    return [Finding(display_path, "archive-total-size-limit")]
                for member in members:
                    if member.is_dir():
                        scan_member(member.filename, b"")
                        continue
                    if member.file_size > _MAX_ARCHIVE_BYTES:
                        scan_member(member.filename, None)
                        continue
                    with archive.open(member) as handle:
                        scan_member(member.filename, _read_limited(handle))
            return findings

        if data.startswith(b"\x1f\x8b"):
            with gzip.GzipFile(fileobj=io.BytesIO(data)) as handle:
                scan_member("<gzip-content>", _read_limited(handle))
            return findings
        if data.startswith(b"BZh"):
            with bz2.BZ2File(io.BytesIO(data)) as handle:
                scan_member("<bzip2-content>", _read_limited(handle))
            return findings
        if data.startswith(b"\xfd7zXZ\x00"):
            with lzma.LZMAFile(io.BytesIO(data)) as handle:
                scan_member("<xz-content>", _read_limited(handle))
            return findings

        if len(data) > 262 and data[257:262] == b"ustar":
            with tarfile.open(fileobj=io.BytesIO(data), mode="r:") as archive:
                members = archive.getmembers()
                if len(members) > _MAX_ARCHIVE_MEMBERS:
                    return [Finding(display_path, "archive-member-count-limit")]
                if sum(member.size for member in members) > _MAX_ARCHIVE_TOTAL_BYTES:
                    return [Finding(display_path, "archive-total-size-limit")]
                for member in members:
                    if member.isdir():
                        continue
                    raw_member_display = f"{display_path}!{member.name}"
                    member_path_hits = _scan_bytes(
                        raw_member_display.encode("utf-8", errors="surrogateescape"),
                        needles,
                    )
                    member_display = _safe_path(raw_member_display, needles)
                    findings.extend(
                        Finding(member_display, f"path:{kind}")
                        for kind in sorted(member_path_hits)
                    )
                    if not member.isfile():
                        findings.append(
                            Finding(member_display, "archive-special-entry")
                        )
                        continue
                    handle = archive.extractfile(member)
                    if handle is None:
                        findings.append(
                            Finding(member_display, "archive-unreadable-entry")
                        )
                        continue
                    with handle:
                        scan_member(member.name, _read_limited(handle))
            return findings
    except (OSError, EOFError, tarfile.TarError, zipfile.BadZipFile, lzma.LZMAError):
        findings.append(Finding(display_path, "archive-decode-error"))

    return findings


def scan_tree(root: Path, needles: Sequence[Needle]) -> tuple[list[Finding], int]:
    findings: list[Finding] = []
    scanned_files = 0
    max_needle = max((len(needle.value) for needle in needles), default=0)
    overlap = max(4096, max_needle + 1)

    for path, entry_type in _iter_tree(root):
        relative = path.name if root.is_file() else str(path.relative_to(root))
        path_hits = _scan_bytes(
            relative.encode("utf-8", errors="surrogateescape"), needles
        )
        safe_relative = _safe_path(relative, needles)
        findings.extend(
            Finding(safe_relative, f"path:{kind}") for kind in sorted(path_hits)
        )
        if entry_type == "symlink":
            findings.append(Finding(safe_relative, "symlink-blocked"))
            continue
        if entry_type == "special":
            findings.append(Finding(safe_relative, "special-file-blocked"))
            continue
        if entry_type == "directory":
            continue

        scanned_files += 1
        found_kinds: set[str] = set()
        tail = b""
        with path.open("rb") as handle:
            while chunk := handle.read(1024 * 1024):
                window = tail + chunk
                found_kinds.update(_scan_bytes(window, needles))
                tail = window[-overlap:]
        findings.extend(Finding(safe_relative, kind) for kind in sorted(found_kinds))

        suffixes = "".join(path.suffixes).lower()
        unsupported = next(
            (
                suffix
                for suffix in _UNSUPPORTED_ARCHIVE_SUFFIXES
                if suffixes.endswith(suffix)
            ),
            None,
        )
        if unsupported:
            findings.append(
                Finding(safe_relative, f"unsupported-archive:{unsupported}")
            )
            continue

        with path.open("rb") as handle:
            prefix = handle.read(263)
        if prefix.startswith(_UNSUPPORTED_ARCHIVE_MAGIC):
            findings.append(Finding(safe_relative, "unsupported-archive:magic"))
            continue
        if _looks_like_archive(prefix, path.suffix):
            if path.stat().st_size > _MAX_ARCHIVE_BYTES:
                findings.append(Finding(safe_relative, "archive-container-size-limit"))
                continue
            findings.extend(
                _scan_archive_blob(path.read_bytes(), safe_relative, needles)
            )

    unique = sorted(set(findings), key=lambda finding: (finding.path, finding.kind))
    return unique, scanned_files


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Block benchmark publication when artifacts contain credentials."
    )
    parser.add_argument("root", type=Path, help="artifact file or directory to scan")
    parser.add_argument(
        "--env-name",
        action="append",
        default=[],
        help="additional environment variable whose value must not appear",
    )
    parser.add_argument(
        "--require-env",
        action="append",
        default=[],
        help="fail if this credential environment variable is unavailable",
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="emit a machine-readable report without secret values or context",
    )
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    missing = sorted(name for name in args.require_env if not os.environ.get(name))
    if missing:
        safe_missing_root = _safe_path(str(args.root), ())
        report = {
            "clean": False,
            "error": "required credential environment unavailable",
            "missing_environment_names": missing,
            "root": safe_missing_root,
        }
        output = json.dumps(report, indent=2) if args.json else report["error"]
        print(output, file=sys.stderr)
        return 2

    needles = environment_needles(
        os.environ,
        [*args.env_name, *args.require_env],
    )
    safe_root = _safe_path(str(args.root), needles)
    try:
        findings, scanned_files = scan_tree(args.root, needles)
    except (FileNotFoundError, OSError) as exc:
        message = {
            "clean": False,
            "error": f"scan failed: {type(exc).__name__}",
            "root": safe_root,
        }
        output = json.dumps(message, indent=2) if args.json else message["error"]
        print(output, file=sys.stderr)
        return 2

    report = {
        "clean": not findings,
        "root": safe_root,
        "files_scanned": scanned_files,
        "credential_variants_loaded": len(needles),
        "findings": [finding.__dict__ for finding in findings],
    }
    if args.json:
        print(json.dumps(report, indent=2, sort_keys=True))
    elif findings:
        print(f"BLOCKED: {len(findings)} credential finding(s); no values shown")
        for finding in findings:
            print(f"{finding.path}: {finding.kind}")
    else:
        print(f"CLEAN: scanned {scanned_files} file(s)")
    return 1 if findings else 0


if __name__ == "__main__":
    raise SystemExit(main())
