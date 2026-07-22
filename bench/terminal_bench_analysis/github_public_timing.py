"""Live, read-only GitHub chronology verification for the Stella TB 2.1 study."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import ssl
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass
from datetime import datetime, timedelta
from pathlib import Path, PurePosixPath
from typing import Any, Protocol

AUDIT_SCHEMA_VERSION = "stella-tb21-github-public-timing-audit-v3"
EVIDENCE_SCHEMA_VERSION = "stella-tb21-github-public-timing-evidence-v2"
ATTESTATION_SCHEMA_VERSION = "stella-tb21-github-attestation-v2"
PUBLICATION_SAFETY_MARGIN_SECONDS = 2
FIXED_REPOSITORY = "macanderson/stella"
FIXED_WEB_ROOT = f"https://github.com/{FIXED_REPOSITORY}"
FIXED_API_ROOT = f"https://api.github.com/repos/{FIXED_REPOSITORY}"
DEFAULT_PROTOCOL_PATH = "bench/terminal-bench-2.1-protocol.md"
DEFAULT_ANALYZER_PATH = "bench/terminal_bench_analysis/tb21_analysis.py"
DEFAULT_PUBLIC_TIMING_PATH = "bench/terminal_bench_analysis/github_public_timing.py"
MAX_GITHUB_RESPONSE_BYTES = 8 * 1024 * 1024

EVIDENCE_FIELDS = frozenset(
    {
        "schema_version",
        "repository",
        "protocol_path",
        "analyzer_path",
        "public_timing_path",
        "manifest_path",
        "issue_url",
        "comments",
        "final_ledger_commit",
    }
)
EVIDENCE_COMMENT_FIELDS = frozenset({"subject_type", "subject_id", "html_url"})
ATTESTATION_COMMON_FIELDS = frozenset(
    {
        "schema_version",
        "study_id",
        "subject_type",
        "subject_id",
        "kind",
        "subject_commit",
        "ledger_commit",
        "ledger_path",
    }
)
COMMENT_URL_RE = re.compile(
    r"https://github\.com/macanderson/stella/issues/(?P<issue>[1-9][0-9]*)"
    r"#issuecomment-(?P<comment>[1-9][0-9]*)"
)
SHA40_RE = re.compile(r"[0-9a-f]{40}")
SHA256_RE = re.compile(r"[0-9a-f]{64}")
ISSUE_URL_RE = re.compile(
    r"https://github\.com/macanderson/stella/issues/(?P<issue>[1-9][0-9]*)"
)


class GitHubReader(Protocol):
    """Minimal read-only API used by the verifier and mocked tests."""

    def get_repository(self) -> dict[str, Any]: ...

    def get_issue(self, issue_number: int) -> dict[str, Any]: ...

    def get_comment(self, comment_id: int) -> dict[str, Any]: ...

    def get_commit(self, commit_sha: str) -> dict[str, Any]: ...

    def get_content(self, path: str, commit_sha: str) -> bytes: ...

    def compare_commits(self, base_sha: str, head_sha: str) -> dict[str, Any]: ...


class GitHubReadError(RuntimeError):
    """A deterministic wrapper for a failed GitHub read."""


def _fixed_system_tls_context() -> ssl.SSLContext:
    """Build public-root TLS without honoring ambient CA override variables."""
    paths = ssl.get_default_verify_paths()
    cafile = (
        paths.openssl_cafile
        if paths.openssl_cafile and Path(paths.openssl_cafile).is_file()
        else None
    )
    capath = (
        paths.openssl_capath
        if paths.openssl_capath and Path(paths.openssl_capath).is_dir()
        else None
    )
    if cafile is None and capath is None:
        raise GitHubReadError(
            "cannot locate the interpreter's compiled public CA roots"
        )
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
    context.check_hostname = True
    context.verify_mode = ssl.CERT_REQUIRED
    context.minimum_version = ssl.TLSVersion.TLSv1_2
    context.load_verify_locations(cafile=cafile, capath=capath)
    return context


def _object_without_duplicate_keys(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    value: dict[str, Any] = {}
    for key, item in pairs:
        if key in value:
            raise ValueError("JSON object contains duplicate keys")
        value[key] = item
    return value


class _NoRedirectHandler(urllib.request.HTTPRedirectHandler):
    """Fail closed instead of following a GitHub API redirect."""

    def redirect_request(  # type: ignore[override]
        self,
        req: urllib.request.Request,
        fp: Any,
        code: int,
        msg: str,
        headers: Any,
        newurl: str,
    ) -> None:
        return None


class GitHubAPIReader:
    """Anonymous GET-only GitHub client isolated from ambient proxy and CA state."""

    def __init__(self) -> None:
        context = _fixed_system_tls_context()
        self._opener = urllib.request.build_opener(
            urllib.request.ProxyHandler({}),
            urllib.request.HTTPSHandler(context=context),
            _NoRedirectHandler(),
        )

    def _read(self, url: str, *, accept: str) -> bytes:
        headers = {
            "Accept": accept,
            "User-Agent": "stella-tb21-public-timing-auditor/1",
            "X-GitHub-Api-Version": "2022-11-28",
        }
        request = urllib.request.Request(url, headers=headers, method="GET")
        if request.has_header("Authorization"):
            raise GitHubReadError("anonymous GitHub request gained authorization")
        try:
            with self._opener.open(request, timeout=30) as response:  # noqa: S310
                status = getattr(response, "status", response.getcode())
                final_url = response.geturl()
                raw = response.read(MAX_GITHUB_RESPONSE_BYTES + 1)
        except (OSError, urllib.error.HTTPError, urllib.error.URLError) as error:
            raise GitHubReadError(f"anonymous GET failed for {url}") from error
        if status != 200 or final_url != url:
            raise GitHubReadError(
                "anonymous GitHub GET did not return the exact requested resource"
            )
        if len(raw) > MAX_GITHUB_RESPONSE_BYTES:
            raise GitHubReadError("anonymous GitHub response exceeded 8 MiB")
        return raw

    def _json(self, url: str) -> dict[str, Any]:
        raw = self._read(url, accept="application/vnd.github+json")
        try:
            return _json_object(raw, label=f"GitHub response for {url}")
        except ValueError as error:
            raise GitHubReadError(
                f"GitHub returned non-strict JSON data for {url}"
            ) from error

    def get_repository(self) -> dict[str, Any]:
        return self._json(FIXED_API_ROOT)

    def get_issue(self, issue_number: int) -> dict[str, Any]:
        return self._json(f"{FIXED_API_ROOT}/issues/{issue_number}")

    def get_comment(self, comment_id: int) -> dict[str, Any]:
        return self._json(f"{FIXED_API_ROOT}/issues/comments/{comment_id}")

    def get_commit(self, commit_sha: str) -> dict[str, Any]:
        return self._json(f"{FIXED_API_ROOT}/commits/{commit_sha}")

    def get_content(self, path: str, commit_sha: str) -> bytes:
        encoded_path = urllib.parse.quote(path, safe="/")
        encoded_ref = urllib.parse.quote(commit_sha, safe="")
        return self._read(
            f"{FIXED_API_ROOT}/contents/{encoded_path}?ref={encoded_ref}",
            accept="application/vnd.github.raw+json",
        )

    def compare_commits(self, base_sha: str, head_sha: str) -> dict[str, Any]:
        encoded_base = urllib.parse.quote(base_sha, safe="")
        encoded_head = urllib.parse.quote(head_sha, safe="")
        return self._json(f"{FIXED_API_ROOT}/compare/{encoded_base}...{encoded_head}")


@dataclass(frozen=True)
class LivePublicTimingAudit:
    """In-process proof that the analyzer generated the report with live GETs."""

    report: dict[str, Any]
    run_ledger_sha256: str
    study_manifest_sha256: str
    evidence_sha256: str


def _sha256(raw: bytes) -> str:
    return hashlib.sha256(raw).hexdigest()


def _canonical_sha256(value: Any) -> str:
    raw = json.dumps(
        value,
        sort_keys=True,
        separators=(",", ":"),
        ensure_ascii=False,
    ).encode("utf-8")
    return _sha256(raw)


def _json_object(raw: bytes, *, label: str) -> dict[str, Any]:
    try:
        value = json.loads(
            raw.decode("utf-8"), object_pairs_hook=_object_without_duplicate_keys
        )
    except (UnicodeDecodeError, json.JSONDecodeError, ValueError) as error:
        raise ValueError(f"{label} is not valid UTF-8 JSON: {error}") from error
    if not isinstance(value, dict):
        raise ValueError(f"{label} must be a JSON object")
    return value


def _safe_repo_path(value: Any) -> str | None:
    if not isinstance(value, str) or not value or "\\" in value:
        return None
    path = PurePosixPath(value)
    if path.is_absolute() or ".." in path.parts or str(path) != value:
        return None
    return value


def _parse_timestamp(value: Any) -> datetime | None:
    if not isinstance(value, str) or not value:
        return None
    normalized = value[:-1] + "+00:00" if value.endswith("Z") else value
    try:
        parsed = datetime.fromisoformat(normalized)
    except ValueError:
        return None
    return parsed if parsed.tzinfo is not None else None


_LEDGER_ARRAY_FIELDS = ("preregistrations", "intents", "publications", "outcomes")


def _ledger_is_prefix(snapshot: dict[str, Any], final: dict[str, Any]) -> bool:
    """Return whether a historical ledger is an exact append-only prefix."""
    if set(snapshot) != set(final):
        return False
    for key, value in final.items():
        if key in _LEDGER_ARRAY_FIELDS:
            historical = snapshot.get(key)
            if not isinstance(value, list) or not isinstance(historical, list):
                return False
            if historical != value[: len(historical)]:
                return False
        elif snapshot.get(key) != value:
            return False
    return True


def _strict_descendant(
    reader: GitHubReader,
    base_sha: str,
    head_sha: str,
) -> tuple[bool, str | None]:
    """Check fixed-repository ancestry through GitHub's compare API."""
    try:
        comparison = reader.compare_commits(base_sha, head_sha)
    except (GitHubReadError, OSError, ValueError) as error:
        return False, str(error)
    commits = comparison.get("commits")
    last_sha = (
        commits[-1].get("sha")
        if isinstance(commits, list) and commits and isinstance(commits[-1], dict)
        else None
    )
    valid = bool(
        comparison.get("status") == "ahead"
        and isinstance(comparison.get("ahead_by"), int)
        and comparison["ahead_by"] > 0
        and isinstance(comparison.get("base_commit"), dict)
        and comparison["base_commit"].get("sha") == base_sha
        and isinstance(comparison.get("merge_base_commit"), dict)
        and comparison["merge_base_commit"].get("sha") == base_sha
        and last_sha == head_sha
    )
    return valid, None if valid else "GitHub compare response did not prove ancestry"


def _finalize(
    report: dict[str, Any],
    errors: list[str],
    *,
    ledger_sha256: str,
    manifest_sha256: str,
    evidence_sha256: str,
) -> LivePublicTimingAudit:
    report["errors"] = list(dict.fromkeys(errors))
    report["valid"] = not report["errors"]
    report["commits"] = sorted(report["commits"], key=lambda item: item["commit_sha"])
    report["publications"] = sorted(
        report["publications"],
        key=lambda item: (item["subject_type"], item["subject_id"]),
    )
    return LivePublicTimingAudit(
        report=report,
        run_ledger_sha256=ledger_sha256,
        study_manifest_sha256=manifest_sha256,
        evidence_sha256=evidence_sha256,
    )


def verify_public_timing(
    run_ledger_path: Path,
    study_manifest_path: Path,
    evidence_path: Path,
    *,
    client: GitHubReader | None = None,
) -> LivePublicTimingAudit:
    """Verify public, immutable chronology with anonymous fixed-repository GETs."""
    ledger_raw = run_ledger_path.read_bytes()
    manifest_raw = study_manifest_path.read_bytes()
    evidence_raw = evidence_path.read_bytes()
    ledger_sha256 = _sha256(ledger_raw)
    manifest_sha256 = _sha256(manifest_raw)
    evidence_sha256 = _sha256(evidence_raw)
    ledger = _json_object(ledger_raw, label="run ledger")
    manifest = _json_object(manifest_raw, label="study manifest")
    evidence = _json_object(evidence_raw, label="GitHub evidence")
    # Deliberately do not read GITHUB_TOKEN. Claim evidence must be anonymously
    # readable and therefore independently auditable.
    reader = client or GitHubAPIReader()
    report: dict[str, Any] = {
        "schema_version": AUDIT_SCHEMA_VERSION,
        "repository": FIXED_REPOSITORY,
        "anonymous_reads": True,
        "valid": False,
        "inputs": {
            "run_ledger_sha256": ledger_sha256,
            "study_manifest_sha256": manifest_sha256,
            "evidence_sha256": evidence_sha256,
        },
        "commits": [],
        "comparisons": [],
        "publications": [],
        "finalization": {},
        "errors": [],
    }
    errors: list[str] = []

    try:
        repository = reader.get_repository()
    except (GitHubReadError, OSError, ValueError) as error:
        repository = {}
        errors.append(f"Fixed repository is not anonymously readable: {error}")
    if (
        repository.get("full_name") != FIXED_REPOSITORY
        or repository.get("private") is not False
    ):
        errors.append("Fixed repository must be public and anonymously readable.")

    if set(evidence) != EVIDENCE_FIELDS:
        errors.append("Evidence top-level fields differ from the exact v2 schema.")
    if evidence.get("schema_version") != EVIDENCE_SCHEMA_VERSION:
        errors.append(f"Evidence schema_version must be {EVIDENCE_SCHEMA_VERSION!r}.")
    if evidence.get("repository") != FIXED_REPOSITORY:
        errors.append(f"Evidence repository must be exactly {FIXED_REPOSITORY!r}.")
    ledger_path = _safe_repo_path(ledger.get("ledger_path"))
    protocol_path = _safe_repo_path(evidence.get("protocol_path"))
    analyzer_path = _safe_repo_path(evidence.get("analyzer_path"))
    public_timing_path = _safe_repo_path(evidence.get("public_timing_path"))
    manifest_repo_path = _safe_repo_path(evidence.get("manifest_path"))
    if ledger_path is None:
        errors.append("Run ledger ledger_path is not a safe repository-relative path.")
    if protocol_path != DEFAULT_PROTOCOL_PATH:
        errors.append(f"Evidence protocol_path must be {DEFAULT_PROTOCOL_PATH!r}.")
    if analyzer_path != DEFAULT_ANALYZER_PATH:
        errors.append(f"Evidence analyzer_path must be {DEFAULT_ANALYZER_PATH!r}.")
    if public_timing_path != DEFAULT_PUBLIC_TIMING_PATH:
        errors.append(
            f"Evidence public_timing_path must be {DEFAULT_PUBLIC_TIMING_PATH!r}."
        )
    if manifest_repo_path is None:
        errors.append("Evidence manifest_path is not a safe repository-relative path.")

    study_id = ledger.get("study_id")
    manifest_prereg = manifest.get("preregistration")
    manifest_study_id = (
        manifest_prereg.get("study_id") if isinstance(manifest_prereg, dict) else None
    )
    if not isinstance(study_id, str) or not study_id:
        errors.append("Run ledger study_id must be non-empty.")
    if manifest_study_id != study_id:
        errors.append("Study manifest and run ledger study_id values differ.")

    analysis_identity = manifest.get("analysis")
    local_analyzer = Path(__file__).with_name("tb21_analysis.py").read_bytes()
    local_public_timing = Path(__file__).read_bytes()
    local_analyzer_sha = _sha256(local_analyzer)
    local_public_timing_sha = _sha256(local_public_timing)
    if not isinstance(analysis_identity, dict) or set(analysis_identity) != {
        "sha256",
        "public_timing_sha256",
    }:
        errors.append("Manifest analysis identity differs from the exact v6 schema.")
        analysis_identity = {}
    if analysis_identity.get("sha256") != local_analyzer_sha:
        errors.append("Manifest analyzer SHA-256 differs from the executing analyzer.")
    if analysis_identity.get("public_timing_sha256") != local_public_timing_sha:
        errors.append(
            "Manifest public-timing SHA-256 differs from the executing verifier."
        )

    arrays: dict[str, list[dict[str, Any]]] = {}
    for name in _LEDGER_ARRAY_FIELDS:
        value = ledger.get(name)
        if not isinstance(value, list) or not all(
            isinstance(item, dict) for item in value
        ):
            errors.append(f"Run ledger {name} must be an array of objects.")
            arrays[name] = []
        else:
            arrays[name] = value
    preregistrations = arrays["preregistrations"]
    intents = arrays["intents"]
    publications = arrays["publications"]
    outcomes = arrays["outcomes"]
    prereg_by_kind = {
        item.get("kind"): item
        for item in preregistrations
        if isinstance(item.get("kind"), str)
    }
    required_kinds = {"readiness", "calibration", "confirmatory_freeze"}
    if set(prereg_by_kind) != required_kinds or len(preregistrations) != 3:
        errors.append("Run ledger must contain exactly the three preregistrations.")
    if (
        prereg_by_kind.get("confirmatory_freeze", {}).get("study_manifest_sha256")
        != manifest_sha256
    ):
        errors.append(
            "Confirmatory preregistration does not bind the exact supplied "
            "manifest-file SHA-256."
        )

    intent_by_sha: dict[str, dict[str, Any]] = {}
    wrapper_by_sha: dict[str, dict[str, Any]] = {}
    for wrapper in intents:
        intent = wrapper.get("intent")
        digest = wrapper.get("intent_sha256")
        if not isinstance(intent, dict) or not isinstance(digest, str):
            continue
        if digest != _canonical_sha256(intent):
            errors.append(
                f"Local intent {digest!r} does not match its canonical SHA-256."
            )
            continue
        if digest in intent_by_sha:
            errors.append(f"Duplicate intent digest {digest!r}.")
        intent_by_sha[digest] = intent
        wrapper_by_sha[digest] = wrapper
    paid_intents = {
        digest: intent
        for digest, intent in intent_by_sha.items()
        if intent.get("historical") is False
        and intent.get("stage") in {"readiness", "calibration", "confirmatory"}
    }
    if sorted(intent.get("stage") for intent in paid_intents.values()) != [
        "calibration",
        "confirmatory",
        "readiness",
    ]:
        errors.append(
            "Run ledger must contain exactly three nonhistorical paid intents."
        )
    outcome_by_sha = {
        item.get("intent_sha256"): item
        for item in outcomes
        if isinstance(item.get("intent_sha256"), str)
    }

    publication_by_subject: dict[tuple[str, str], dict[str, Any]] = {}
    publication_fields = {
        "sequence",
        "subject_type",
        "subject_id",
        "ledger_commit",
        "public_url",
        "published_at",
    }
    for publication in publications:
        if set(publication) != publication_fields:
            errors.append("A publication record differs from the exact v2 schema.")
        key = (publication.get("subject_type"), publication.get("subject_id"))
        if not all(isinstance(item, str) for item in key):
            continue
        if key in publication_by_subject:
            errors.append(f"Duplicate publication record for {key!r}.")
        publication_by_subject[key] = publication
    required_subjects = {
        *(("preregistration", kind) for kind in required_kinds),
        *(("intent", digest) for digest in paid_intents),
    }
    if set(publication_by_subject) != required_subjects:
        errors.append(
            "Run ledger publications must exactly equal three preregistrations and "
            "three paid intents."
        )

    evidence_comments = evidence.get("comments")
    evidence_by_subject: dict[tuple[str, str], dict[str, Any]] = {}
    if not isinstance(evidence_comments, list):
        errors.append("Evidence comments must be an array.")
        evidence_comments = []
    for item in evidence_comments:
        if not isinstance(item, dict) or set(item) != EVIDENCE_COMMENT_FIELDS:
            errors.append("An evidence comment differs from the exact v2 schema.")
            continue
        key = (item.get("subject_type"), item.get("subject_id"))
        if not all(isinstance(value, str) for value in key):
            continue
        if key in evidence_by_subject:
            errors.append(f"Duplicate evidence comment for {key!r}.")
        evidence_by_subject[key] = item
    if set(evidence_by_subject) != required_subjects:
        errors.append("Evidence comments do not exactly cover every publication.")

    issue_url = evidence.get("issue_url")
    issue_match = (
        ISSUE_URL_RE.fullmatch(issue_url) if isinstance(issue_url, str) else None
    )
    issue_number = int(issue_match.group("issue")) if issue_match else None
    if issue_number is None:
        errors.append("Evidence issue_url is not an exact fixed-repository issue URL.")
    else:
        try:
            issue = reader.get_issue(issue_number)
        except (GitHubReadError, OSError, ValueError) as error:
            issue = {}
            errors.append(f"Dedicated preregistration issue is unreadable: {error}")
        if (
            issue.get("html_url") != issue_url
            or issue.get("title")
            != f"Stella Terminal-Bench 2.1 preregistration: {study_id}"
            or not isinstance(issue.get("user"), dict)
            or issue["user"].get("login") != "macanderson"
            or issue.get("author_association") != "OWNER"
        ):
            errors.append(
                "Evidence issue is not the dedicated owner-authored prereg issue."
            )

    subject_commit_by_key: dict[tuple[str, str], str | None] = {}
    for key in required_subjects:
        subject_type, subject_id = key
        if subject_type == "preregistration":
            subject_commit_by_key[key] = prereg_by_kind.get(subject_id, {}).get(
                "commit"
            )
        else:
            subject_commit_by_key[key] = paid_intents.get(subject_id, {}).get(
                "preregistration_commit"
            )
    ledger_commit_by_key = {
        key: publication_by_subject.get(key, {}).get("ledger_commit")
        for key in required_subjects
    }
    all_commits = {
        value
        for value in [*subject_commit_by_key.values(), *ledger_commit_by_key.values()]
        if isinstance(value, str)
    }

    final_ledger_commit = evidence.get("final_ledger_commit")
    if (
        not isinstance(final_ledger_commit, str)
        or SHA40_RE.fullmatch(final_ledger_commit) is None
    ):
        errors.append("Evidence final_ledger_commit must be one full lowercase SHA.")
    if isinstance(final_ledger_commit, str):
        all_commits.add(final_ledger_commit)

    content_by_commit: dict[str, dict[str, bytes]] = {}
    committed_ledgers: dict[str, dict[str, Any]] = {}
    for commit_sha in sorted(all_commits):
        before = len(errors)
        if SHA40_RE.fullmatch(commit_sha) is None:
            errors.append(f"Referenced commit {commit_sha!r} is not a lowercase SHA.")
            continue
        expected_commit_url = f"{FIXED_WEB_ROOT}/commit/{commit_sha}"
        try:
            commit_record = reader.get_commit(commit_sha)
        except (GitHubReadError, OSError, ValueError) as error:
            commit_record = {}
            errors.append(f"Commit {commit_sha} is not anonymously readable: {error}")
        if (
            commit_record.get("sha") != commit_sha
            or commit_record.get("html_url") != expected_commit_url
        ):
            errors.append(f"Commit API identity differs for {commit_sha}.")
        required_paths = [protocol_path, analyzer_path, public_timing_path]
        if (
            commit_sha in ledger_commit_by_key.values()
            or commit_sha == final_ledger_commit
        ):
            required_paths.append(ledger_path)
        freeze_subject = prereg_by_kind.get("confirmatory_freeze", {}).get("commit")
        if commit_sha == freeze_subject:
            required_paths.append(manifest_repo_path)
        files: dict[str, bytes] = {}
        digests: dict[str, dict[str, Any]] = {}
        for repo_path in dict.fromkeys(required_paths):
            if repo_path is None:
                continue
            try:
                raw = reader.get_content(repo_path, commit_sha)
            except (GitHubReadError, OSError, ValueError) as error:
                errors.append(
                    f"Commit {commit_sha} is missing required content "
                    f"{repo_path!r}: {error}"
                )
                continue
            files[repo_path] = raw
            digests[repo_path] = {"sha256": _sha256(raw), "size": len(raw)}
        content_by_commit[commit_sha] = files
        if (
            public_timing_path in files
            and files[public_timing_path] != local_public_timing
        ):
            errors.append(
                f"Public-timing verifier bytes at {commit_sha} differ from the "
                "executing verifier."
            )
        if analyzer_path in files and _sha256(
            files[analyzer_path]
        ) != analysis_identity.get("sha256"):
            errors.append(
                f"Analyzer bytes at {commit_sha} differ from manifest identity."
            )
        if ledger_path in files:
            try:
                committed_ledgers[commit_sha] = _json_object(
                    files[ledger_path], label=f"ledger at commit {commit_sha}"
                )
            except ValueError as error:
                errors.append(str(error))
        if (
            commit_sha == freeze_subject
            and manifest_repo_path in files
            and files[manifest_repo_path] != manifest_raw
        ):
            errors.append(
                "Confirmatory subject freeze does not contain exact supplied "
                "manifest bytes."
            )
        report["commits"].append(
            {
                "commit_sha": commit_sha,
                "html_url": expected_commit_url,
                "files": digests,
                "verified": len(errors) == before,
            }
        )

    comparison_cache: dict[tuple[str, str], bool] = {}

    def require_descendant(base: Any, head: Any, label: str) -> bool:
        if not isinstance(base, str) or not isinstance(head, str) or base == head:
            errors.append(f"{label} requires two distinct full commit SHAs.")
            return False
        key = (base, head)
        if key not in comparison_cache:
            valid, detail = _strict_descendant(reader, base, head)
            comparison_cache[key] = valid
            report["comparisons"].append(
                {"base": base, "head": head, "label": label, "verified": valid}
            )
            if not valid:
                errors.append(f"{label} ancestry is not proven: {detail}.")
        elif not comparison_cache[key]:
            errors.append(f"{label} ancestry is not proven.")
        return comparison_cache[key]

    ordered_publications = sorted(
        publication_by_subject.items(),
        key=lambda item: (
            item[1].get("sequence")
            if isinstance(item[1].get("sequence"), int)
            else 10**18
        ),
    )
    for (_, prior), (_, current) in zip(
        ordered_publications, ordered_publications[1:], strict=False
    ):
        prior_commit = prior.get("ledger_commit")
        current_commit = current.get("ledger_commit")
        if prior_commit != current_commit:
            require_descendant(
                prior_commit, current_commit, "Publication snapshot sequence"
            )

    seen_comment_ids: set[int] = set()
    for key in sorted(required_subjects):
        start_errors = len(errors)
        subject_type, subject_id = key
        publication = publication_by_subject.get(key, {})
        evidence_comment = evidence_by_subject.get(key, {})
        subject_commit = subject_commit_by_key.get(key)
        ledger_commit = ledger_commit_by_key.get(key)
        require_descendant(subject_commit, ledger_commit, f"Publication {key!r}")
        expected_public_url = (
            f"{FIXED_WEB_ROOT}/commit/{ledger_commit}"
            if isinstance(ledger_commit, str)
            else None
        )
        if publication.get("public_url") != expected_public_url:
            errors.append(f"Publication {key!r} URL does not name its ledger snapshot.")
        snapshot = committed_ledgers.get(ledger_commit, {})
        if not _ledger_is_prefix(snapshot, ledger):
            errors.append(
                f"Publication {key!r} ledger snapshot is not an exact prefix."
            )

        if subject_type == "preregistration":
            payload = prereg_by_kind.get(subject_id)
            kind = subject_id
            digest_field = "canonical_payload_sha256"
            digest = _canonical_sha256(payload) if isinstance(payload, dict) else None
            target_list = "preregistrations"
            target = payload
            stage = (
                "confirmatory" if subject_id == "confirmatory_freeze" else subject_id
            )
            matching_intent_sha = next(
                (
                    sha
                    for sha, intent in paid_intents.items()
                    if intent.get("stage") == stage
                ),
                None,
            )
            outcome = outcome_by_sha.get(matching_intent_sha, {})
        else:
            payload = paid_intents.get(subject_id)
            kind = payload.get("stage") if isinstance(payload, dict) else None
            digest_field = "intent_sha256"
            digest = subject_id
            target_list = "intents"
            target = wrapper_by_sha.get(subject_id)
            outcome = outcome_by_sha.get(subject_id, {})
        committed_targets = snapshot.get(target_list)
        if not isinstance(committed_targets, list) or target not in committed_targets:
            errors.append(f"Bound payload {key!r} is absent from ledger snapshot.")
        if subject_type == "intent" and isinstance(payload, dict):
            artifacts = payload.get("artifacts")
            artifacts = artifacts if isinstance(artifacts, dict) else {}
            for commit_sha in (subject_commit, ledger_commit):
                files = content_by_commit.get(commit_sha, {})
                if _sha256(files.get(analyzer_path, b"")) != artifacts.get(
                    "analysis_sha256"
                ):
                    errors.append(
                        f"Intent {subject_id!r} does not bind analyzer bytes at "
                        f"{commit_sha}."
                    )
                if _sha256(files.get(public_timing_path, b"")) != artifacts.get(
                    "public_timing_sha256"
                ):
                    errors.append(
                        f"Intent {subject_id!r} does not bind public verifier bytes "
                        f"at {commit_sha}."
                    )

        html_url = evidence_comment.get("html_url")
        match = (
            COMMENT_URL_RE.fullmatch(html_url) if isinstance(html_url, str) else None
        )
        comment: dict[str, Any] = {}
        comment_id: int | None = None
        if (
            match is None
            or issue_number is None
            or int(match.group("issue")) != issue_number
        ):
            errors.append(f"Evidence {key!r} is not on the dedicated prereg issue.")
        else:
            comment_id = int(match.group("comment"))
            if comment_id in seen_comment_ids:
                errors.append(f"Evidence {key!r} reuses a comment ID.")
            seen_comment_ids.add(comment_id)
            try:
                comment = reader.get_comment(comment_id)
            except (GitHubReadError, OSError, ValueError) as error:
                errors.append(f"Evidence {key!r} comment is unreadable: {error}")
            if (
                comment.get("id") != comment_id
                or comment.get("html_url") != html_url
                or comment.get("issue_url") != f"{FIXED_API_ROOT}/issues/{issue_number}"
                or not isinstance(comment.get("user"), dict)
                or comment["user"].get("login") != "macanderson"
                or comment.get("author_association") != "OWNER"
            ):
                errors.append(
                    f"Evidence {key!r} is not an owner-authored fixed-issue comment."
                )
        created_at = comment.get("created_at")
        if created_at != comment.get("updated_at"):
            errors.append(f"Evidence {key!r} was edited after creation.")
        if created_at != publication.get("published_at"):
            errors.append(f"Evidence {key!r} server timestamp differs from ledger.")
        expected_body = {
            "schema_version": ATTESTATION_SCHEMA_VERSION,
            "study_id": study_id,
            "subject_type": subject_type,
            "subject_id": subject_id,
            "kind": kind,
            "subject_commit": subject_commit,
            "ledger_commit": ledger_commit,
            "ledger_path": ledger_path,
            digest_field: digest,
        }
        try:
            body_text = comment.get("body")
            comment_body = _json_object(
                body_text.encode("utf-8") if isinstance(body_text, str) else b"",
                label=f"evidence {key!r} comment body",
            )
        except ValueError:
            comment_body = None
        if (
            comment_body != expected_body
            or not isinstance(comment_body, dict)
            or set(comment_body) != ATTESTATION_COMMON_FIELDS | {digest_field}
        ):
            errors.append(f"Evidence {key!r} body does not exactly bind the payload.")
        created_time = _parse_timestamp(created_at)
        started_time = _parse_timestamp(outcome.get("started_at"))
        if (
            created_time is None
            or started_time is None
            or created_time + timedelta(seconds=PUBLICATION_SAFETY_MARGIN_SECONDS)
            > started_time
        ):
            errors.append(f"Evidence {key!r} lacks the conservative pre-run margin.")
        report["publications"].append(
            {
                "subject_type": subject_type,
                "subject_id": subject_id,
                "kind": kind,
                "subject_commit": subject_commit,
                "ledger_commit": ledger_commit,
                "comment_id": comment_id,
                "html_url": html_url,
                "server_created_at": created_at,
                "body_sha256": (
                    _sha256(comment["body"].encode("utf-8"))
                    if isinstance(comment.get("body"), str)
                    else None
                ),
                "outcome_started_at": outcome.get("started_at"),
                "payload_sha256": digest,
                "verified": len(errors) == start_errors,
            }
        )

    paid_outcomes = [outcome_by_sha.get(digest) for digest in paid_intents]
    paid_outcomes = [item for item in paid_outcomes if isinstance(item, dict)]
    if len(paid_outcomes) != 3 or any(
        not isinstance(item.get("job_id"), str)
        or SHA256_RE.fullmatch(str(item.get("artifact_tree_sha256"))) is None
        for item in paid_outcomes
    ):
        errors.append(
            "Final ledger does not contain all three paid outcome job/artifact "
            "bindings."
        )
    final_snapshot = committed_ledgers.get(final_ledger_commit, {})
    if (
        final_snapshot != ledger
        or content_by_commit.get(final_ledger_commit, {}).get(ledger_path) != ledger_raw
    ):
        errors.append(
            "Final ledger commit does not contain exact supplied ledger bytes."
        )
    if ordered_publications:
        require_descendant(
            ordered_publications[-1][1].get("ledger_commit"),
            final_ledger_commit,
            "Final ledger snapshot",
        )
    report["finalization"] = {
        "final_ledger_commit_sha": final_ledger_commit,
        "final_ledger_sha256": ledger_sha256,
        "verified": final_snapshot == ledger,
    }
    return _finalize(
        report,
        errors,
        ledger_sha256=ledger_sha256,
        manifest_sha256=manifest_sha256,
        evidence_sha256=evidence_sha256,
    )


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--run-ledger", required=True, type=Path)
    parser.add_argument("--study-manifest", required=True, type=Path)
    parser.add_argument("--evidence", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    try:
        audit = verify_public_timing(
            args.run_ledger,
            args.study_manifest,
            args.evidence,
        )
    except (OSError, ValueError, json.JSONDecodeError) as error:
        raise SystemExit(f"public timing verification failed: {error}") from error
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(
        json.dumps(audit.report, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )
    print(args.output.resolve())
    return 0 if audit.report["valid"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
