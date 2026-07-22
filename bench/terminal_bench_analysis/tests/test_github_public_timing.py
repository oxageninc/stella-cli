from __future__ import annotations

import copy
import hashlib
import json
import urllib.request
from pathlib import Path
from types import SimpleNamespace
from typing import Any

import pytest

import github_public_timing as timing_module
from github_public_timing import (
    ATTESTATION_SCHEMA_VERSION,
    DEFAULT_ANALYZER_PATH,
    DEFAULT_PROTOCOL_PATH,
    DEFAULT_PUBLIC_TIMING_PATH,
    EVIDENCE_SCHEMA_VERSION,
    FIXED_API_ROOT,
    FIXED_REPOSITORY,
    FIXED_WEB_ROOT,
    MAX_GITHUB_RESPONSE_BYTES,
    GitHubAPIReader,
    GitHubReadError,
    verify_public_timing,
)

_LEDGER_PATH = "bench/evidence/stella-tb21-run-ledger.json"
_MANIFEST_PATH = "bench/evidence/stella-tb21-study-manifest.json"
_ISSUE = 7
_SUBJECTS = {"readiness": "1" * 40, "calibration": "4" * 40, "freeze": "7" * 40}
_SNAPSHOTS = ["2" * 40, "3" * 40, "5" * 40, "6" * 40, "8" * 40, "9" * 40]
_FINAL = "a" * 40
_ORDER = [
    _SUBJECTS["readiness"],
    _SNAPSHOTS[0],
    _SNAPSHOTS[1],
    _SUBJECTS["calibration"],
    _SNAPSHOTS[2],
    _SNAPSHOTS[3],
    _SUBJECTS["freeze"],
    _SNAPSHOTS[4],
    _SNAPSHOTS[5],
    _FINAL,
]
_ANALYZER_BYTES = Path(__file__).parents[1].joinpath("tb21_analysis.py").read_bytes()
_VERIFIER_BYTES = (
    Path(__file__).parents[1].joinpath("github_public_timing.py").read_bytes()
)


def _sha(raw: bytes) -> str:
    return hashlib.sha256(raw).hexdigest()


def _canonical(value: Any) -> str:
    return _sha(
        json.dumps(
            value, sort_keys=True, separators=(",", ":"), ensure_ascii=False
        ).encode()
    )


def _bytes(value: dict[str, Any]) -> bytes:
    return (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()


class FakeGitHub:
    def __init__(self) -> None:
        self.repository = {"full_name": FIXED_REPOSITORY, "private": False}
        self.issue = {
            "html_url": f"{FIXED_WEB_ROOT}/issues/{_ISSUE}",
            "title": "Stella Terminal-Bench 2.1 preregistration: study-v2",
            "user": {"login": "macanderson"},
            "author_association": "OWNER",
        }
        self.comments: dict[int, dict[str, Any]] = {}
        self.contents: dict[tuple[str, str], bytes] = {}

    def get_repository(self) -> dict[str, Any]:
        return copy.deepcopy(self.repository)

    def get_issue(self, issue_number: int) -> dict[str, Any]:
        if issue_number != _ISSUE:
            raise GitHubReadError("missing issue")
        return copy.deepcopy(self.issue)

    def get_comment(self, comment_id: int) -> dict[str, Any]:
        try:
            return copy.deepcopy(self.comments[comment_id])
        except KeyError as error:
            raise GitHubReadError("missing comment") from error

    def get_commit(self, commit_sha: str) -> dict[str, Any]:
        if commit_sha not in _ORDER:
            raise GitHubReadError("missing commit")
        return {"sha": commit_sha, "html_url": f"{FIXED_WEB_ROOT}/commit/{commit_sha}"}

    def get_content(self, path: str, commit_sha: str) -> bytes:
        try:
            return self.contents[(path, commit_sha)]
        except KeyError as error:
            raise GitHubReadError(f"missing {path} at {commit_sha}") from error

    def compare_commits(self, base_sha: str, head_sha: str) -> dict[str, Any]:
        base = _ORDER.index(base_sha)
        head = _ORDER.index(head_sha)
        commits = [{"sha": sha} for sha in _ORDER[base + 1 : head + 1]]
        return {
            "status": "ahead" if head > base else "diverged",
            "ahead_by": max(0, head - base),
            "base_commit": {"sha": base_sha},
            "merge_base_commit": {"sha": base_sha if head > base else head_sha},
            "commits": commits,
        }


def _fixture(tmp_path: Path) -> tuple[Path, Path, Path, dict[str, Any], FakeGitHub]:
    manifest = {
        "preregistration": {"study_id": "study-v2"},
        "analysis": {
            "sha256": _sha(_ANALYZER_BYTES),
            "public_timing_sha256": _sha(_VERIFIER_BYTES),
        },
    }
    manifest_path = tmp_path / "manifest.json"
    manifest_path.write_bytes(_bytes(manifest))
    manifest_sha = _sha(manifest_path.read_bytes())
    preregs = [
        {
            "sequence": 1,
            "kind": "readiness",
            "commit": _SUBJECTS["readiness"],
            "study_manifest_sha256": None,
            "declared_at": "2026-01-01T00:00:00Z",
        },
        {
            "sequence": 5,
            "kind": "calibration",
            "commit": _SUBJECTS["calibration"],
            "study_manifest_sha256": None,
            "declared_at": "2026-01-01T00:02:00Z",
        },
        {
            "sequence": 9,
            "kind": "confirmatory_freeze",
            "commit": _SUBJECTS["freeze"],
            "study_manifest_sha256": manifest_sha,
            "declared_at": "2026-01-01T00:04:00Z",
        },
    ]
    stages = [
        (
            "readiness",
            "readiness",
            _SUBJECTS["readiness"],
            "2026-01-01T00:01:00Z",
            "2026-01-01T00:01:30Z",
        ),
        (
            "calibration",
            "calibration",
            _SUBJECTS["calibration"],
            "2026-01-01T00:03:00Z",
            "2026-01-01T00:03:30Z",
        ),
        (
            "confirmatory",
            "confirmatory_freeze",
            _SUBJECTS["freeze"],
            "2026-01-01T00:05:00Z",
            "2026-01-01T00:05:30Z",
        ),
    ]
    wrappers: list[dict[str, Any]] = []
    outcomes: list[dict[str, Any]] = []
    for index, (stage, _, subject, started, completed) in enumerate(stages):
        intent = {
            "intent_id": stage,
            "stage": stage,
            "historical": False,
            "artifacts": {
                "analysis_sha256": _sha(_ANALYZER_BYTES),
                "public_timing_sha256": _sha(_VERIFIER_BYTES),
            },
            "preregistration_commit": subject,
        }
        digest = _canonical(intent)
        wrappers.append(
            {"sequence": 3 + index * 4, "intent": intent, "intent_sha256": digest}
        )
        outcomes.append(
            {
                "sequence": 13 + index,
                "intent_sha256": digest,
                "job_id": f"{stage}-job",
                "started_at": started,
                "completed_at": completed,
                "artifact_tree_sha256": str(index + 1) * 64,
            }
        )
    publications: list[dict[str, Any]] = []
    comment_specs: list[tuple[str, str, str, str, str]] = []
    times = [
        "2026-01-01T00:00:10Z",
        "2026-01-01T00:00:20Z",
        "2026-01-01T00:02:10Z",
        "2026-01-01T00:02:20Z",
        "2026-01-01T00:04:10Z",
        "2026-01-01T00:04:20Z",
    ]
    for index, (stage, prereg_kind, subject, _, _) in enumerate(stages):
        intent_digest = wrappers[index]["intent_sha256"]
        comment_specs.extend(
            [
                (
                    "preregistration",
                    prereg_kind,
                    prereg_kind,
                    subject,
                    times[index * 2],
                ),
                ("intent", intent_digest, stage, subject, times[index * 2 + 1]),
            ]
        )
    client = FakeGitHub()
    evidence_comments: list[dict[str, Any]] = []
    for index, (
        subject_type,
        subject_id,
        kind,
        subject_commit,
        published_at,
    ) in enumerate(comment_specs):
        ledger_commit = _SNAPSHOTS[index]
        publication = {
            "sequence": 2 + index * 2,
            "subject_type": subject_type,
            "subject_id": subject_id,
            "ledger_commit": ledger_commit,
            "public_url": f"{FIXED_WEB_ROOT}/commit/{ledger_commit}",
            "published_at": published_at,
        }
        publications.append(publication)
        comment_id = 101 + index
        html_url = f"{FIXED_WEB_ROOT}/issues/{_ISSUE}#issuecomment-{comment_id}"
        evidence_comments.append(
            {
                "subject_type": subject_type,
                "subject_id": subject_id,
                "html_url": html_url,
            }
        )
        prereg = next((item for item in preregs if item["kind"] == subject_id), None)
        digest_field = (
            "canonical_payload_sha256"
            if subject_type == "preregistration"
            else "intent_sha256"
        )
        digest = _canonical(prereg) if prereg is not None else subject_id
        body = {
            "schema_version": ATTESTATION_SCHEMA_VERSION,
            "study_id": "study-v2",
            "subject_type": subject_type,
            "subject_id": subject_id,
            "kind": kind,
            "subject_commit": subject_commit,
            "ledger_commit": ledger_commit,
            "ledger_path": _LEDGER_PATH,
            digest_field: digest,
        }
        client.comments[comment_id] = {
            "id": comment_id,
            "html_url": html_url,
            "issue_url": f"{FIXED_API_ROOT}/issues/{_ISSUE}",
            "created_at": published_at,
            "updated_at": published_at,
            "user": {"login": "macanderson"},
            "author_association": "OWNER",
            "body": json.dumps(body, sort_keys=True, separators=(",", ":")),
        }
    ledger = {
        "schema_version": "stella-tb21-run-ledger-v2",
        "study_id": "study-v2",
        "ledger_path": _LEDGER_PATH,
        "historical_spend_disclosure": {"known_lower_bound_usd": 0.2},
        "preregistrations": preregs,
        "intents": wrappers,
        "publications": publications,
        "outcomes": outcomes,
    }
    prefixes = [(1, 0), (1, 1), (2, 1), (2, 2), (3, 2), (3, 3)]
    for commit, (prereg_count, intent_count) in zip(_SNAPSHOTS, prefixes, strict=True):
        snapshot = copy.deepcopy(ledger)
        snapshot["preregistrations"] = preregs[:prereg_count]
        snapshot["intents"] = wrappers[:intent_count]
        snapshot["publications"] = []
        snapshot["outcomes"] = []
        client.contents[(_LEDGER_PATH, commit)] = _bytes(snapshot)
    ledger_path = tmp_path / "ledger.json"
    ledger_path.write_bytes(_bytes(ledger))
    client.contents[(_LEDGER_PATH, _FINAL)] = ledger_path.read_bytes()
    for commit in _ORDER:
        client.contents[(DEFAULT_PROTOCOL_PATH, commit)] = b"# protocol\n"
        client.contents[(DEFAULT_ANALYZER_PATH, commit)] = _ANALYZER_BYTES
        client.contents[(DEFAULT_PUBLIC_TIMING_PATH, commit)] = _VERIFIER_BYTES
    client.contents[(_MANIFEST_PATH, _SUBJECTS["freeze"])] = manifest_path.read_bytes()
    evidence = {
        "schema_version": EVIDENCE_SCHEMA_VERSION,
        "repository": FIXED_REPOSITORY,
        "protocol_path": DEFAULT_PROTOCOL_PATH,
        "analyzer_path": DEFAULT_ANALYZER_PATH,
        "public_timing_path": DEFAULT_PUBLIC_TIMING_PATH,
        "manifest_path": _MANIFEST_PATH,
        "issue_url": f"{FIXED_WEB_ROOT}/issues/{_ISSUE}",
        "comments": evidence_comments,
        "final_ledger_commit": _FINAL,
    }
    evidence_path = tmp_path / "evidence.json"
    evidence_path.write_bytes(_bytes(evidence))
    return ledger_path, manifest_path, evidence_path, ledger, client


def test_distinct_snapshot_commits_remove_self_reference(tmp_path: Path) -> None:
    ledger, manifest, evidence, _, client = _fixture(tmp_path)
    first = verify_public_timing(ledger, manifest, evidence, client=client)
    second = verify_public_timing(ledger, manifest, evidence, client=client)
    assert first.report == second.report
    assert first.report["valid"] is True
    assert all(
        item["subject_commit"] != item["ledger_commit"]
        for item in first.report["publications"]
    )


def test_non_descendant_snapshot_fails_closed(tmp_path: Path) -> None:
    ledger, manifest, evidence, _, client = _fixture(tmp_path)
    original = client.compare_commits
    client.compare_commits = lambda base, head: (
        {"status": "diverged"} if head == _SNAPSHOTS[2] else original(base, head)
    )  # type: ignore[method-assign]
    result = verify_public_timing(ledger, manifest, evidence, client=client)
    assert result.report["valid"] is False
    assert any("ancestry is not proven" in error for error in result.report["errors"])


def test_snapshot_must_be_exact_final_ledger_prefix(tmp_path: Path) -> None:
    ledger, manifest, evidence, final_ledger, client = _fixture(tmp_path)
    mutated = copy.deepcopy(final_ledger)
    mutated["study_id"] = "rewritten"
    client.contents[(_LEDGER_PATH, _SNAPSHOTS[0])] = _bytes(mutated)
    result = verify_public_timing(ledger, manifest, evidence, client=client)
    assert result.report["valid"] is False
    assert any("not an exact prefix" in error for error in result.report["errors"])


def test_final_commit_must_contain_exact_completed_ledger(tmp_path: Path) -> None:
    ledger, manifest, evidence, final_ledger, client = _fixture(tmp_path)
    mutated = copy.deepcopy(final_ledger)
    mutated["outcomes"] = mutated["outcomes"][:-1]
    client.contents[(_LEDGER_PATH, _FINAL)] = _bytes(mutated)
    result = verify_public_timing(ledger, manifest, evidence, client=client)
    assert result.report["valid"] is False
    assert any(
        "exact supplied ledger bytes" in error for error in result.report["errors"]
    )


def test_confirmatory_subject_binds_exact_manifest_file(tmp_path: Path) -> None:
    ledger_path, manifest, evidence, ledger, client = _fixture(tmp_path)
    ledger["preregistrations"][2]["study_manifest_sha256"] = "0" * 64
    ledger_path.write_bytes(_bytes(ledger))
    client.contents[(_LEDGER_PATH, _FINAL)] = ledger_path.read_bytes()

    result = verify_public_timing(ledger_path, manifest, evidence, client=client)

    assert result.report["valid"] is False
    assert any("manifest-file SHA-256" in error for error in result.report["errors"])


def test_executing_verifier_bytes_must_match_every_public_commit(
    tmp_path: Path,
) -> None:
    ledger, manifest, evidence, _, client = _fixture(tmp_path)
    client.contents[(DEFAULT_PUBLIC_TIMING_PATH, _SNAPSHOTS[3])] = b"modified\n"
    result = verify_public_timing(ledger, manifest, evidence, client=client)
    assert result.report["valid"] is False
    assert any("executing verifier" in error for error in result.report["errors"])


def test_private_repository_fails_closed(tmp_path: Path) -> None:
    ledger, manifest, evidence, _, client = _fixture(tmp_path)
    client.repository["private"] = True
    result = verify_public_timing(ledger, manifest, evidence, client=client)
    assert result.report["valid"] is False
    assert any("public and anonymously" in error for error in result.report["errors"])


def test_comments_must_share_dedicated_owner_issue(tmp_path: Path) -> None:
    ledger, manifest, evidence, _, client = _fixture(tmp_path)
    client.comments[101]["author_association"] = "CONTRIBUTOR"
    result = verify_public_timing(ledger, manifest, evidence, client=client)
    assert result.report["valid"] is False
    assert any("owner-authored" in error for error in result.report["errors"])


def test_edited_comment_fails_closed(tmp_path: Path) -> None:
    ledger, manifest, evidence, _, client = _fixture(tmp_path)
    client.comments[101]["updated_at"] = "2026-01-01T00:00:11Z"
    result = verify_public_timing(ledger, manifest, evidence, client=client)
    assert result.report["valid"] is False
    assert any("edited after creation" in error for error in result.report["errors"])


def test_duplicate_comment_body_keys_fail_closed(tmp_path: Path) -> None:
    ledger, manifest, evidence, _, client = _fixture(tmp_path)
    canonical = client.comments[101]["body"]
    client.comments[101]["body"] = '{"study_id":"wrong",' + canonical[1:]

    result = verify_public_timing(ledger, manifest, evidence, client=client)

    assert result.report["valid"] is False
    assert any(
        "body does not exactly bind" in error for error in result.report["errors"]
    )


def test_conservative_comment_margin_is_required(tmp_path: Path) -> None:
    ledger_path, manifest, evidence, ledger, client = _fixture(tmp_path)
    ledger["outcomes"][0]["started_at"] = "2026-01-01T00:00:21Z"
    ledger_path.write_bytes(_bytes(ledger))
    client.contents[(_LEDGER_PATH, _FINAL)] = ledger_path.read_bytes()
    result = verify_public_timing(ledger_path, manifest, evidence, client=client)
    assert result.report["valid"] is False
    assert any(
        "conservative pre-run margin" in error for error in result.report["errors"]
    )


class _HTTPResponse:
    def __init__(self, *, url: str, raw: bytes, status: int = 200) -> None:
        self.status = status
        self._url = url
        self._raw = raw
        self.read_limit: int | None = None

    def __enter__(self) -> _HTTPResponse:
        return self

    def __exit__(self, *_: object) -> None:
        return None

    def getcode(self) -> int:
        return self.status

    def geturl(self) -> str:
        return self._url

    def read(self, limit: int) -> bytes:
        self.read_limit = limit
        return self._raw[:limit]


class _Opener:
    def __init__(self, response: _HTTPResponse) -> None:
        self.response = response
        self.request: urllib.request.Request | None = None
        self.timeout: int | None = None

    def open(self, request: urllib.request.Request, *, timeout: int) -> _HTTPResponse:
        self.request = request
        self.timeout = timeout
        return self.response


def _reader_with_response(response: _HTTPResponse) -> tuple[GitHubAPIReader, _Opener]:
    reader = object.__new__(GitHubAPIReader)
    opener = _Opener(response)
    reader._opener = opener  # type: ignore[attr-defined]
    return reader, opener


def test_public_reader_disables_ambient_proxy_and_redirects(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    context = object()
    captured: list[object] = []

    monkeypatch.setattr(timing_module, "_fixed_system_tls_context", lambda: context)

    def capture_opener(*handlers: object) -> object:
        captured.extend(handlers)
        return object()

    monkeypatch.setattr(timing_module.urllib.request, "build_opener", capture_opener)

    GitHubAPIReader()

    proxy = next(
        item for item in captured if isinstance(item, urllib.request.ProxyHandler)
    )
    https = next(
        item for item in captured if isinstance(item, urllib.request.HTTPSHandler)
    )
    assert proxy.proxies == {}
    assert https._context is context
    assert any(isinstance(item, timing_module._NoRedirectHandler) for item in captured)


def test_public_reader_uses_compiled_ca_path_not_ambient_override(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    compiled_ca = tmp_path / "compiled-ca.pem"
    compiled_ca.write_text("public roots", encoding="utf-8")
    ambient_ca = tmp_path / "ambient-ca.pem"
    ambient_ca.write_text("untrusted override", encoding="utf-8")
    monkeypatch.setattr(
        timing_module.ssl,
        "get_default_verify_paths",
        lambda: SimpleNamespace(
            openssl_cafile=str(compiled_ca),
            openssl_capath=None,
            cafile=str(ambient_ca),
            capath=None,
        ),
    )

    class Context:
        def __init__(self, protocol: object) -> None:
            self.protocol = protocol
            self.loaded: tuple[str | None, str | None] | None = None

        def load_verify_locations(
            self, *, cafile: str | None, capath: str | None
        ) -> None:
            self.loaded = (cafile, capath)

    context = Context(timing_module.ssl.PROTOCOL_TLS_CLIENT)
    monkeypatch.setattr(timing_module.ssl, "SSLContext", lambda _: context)

    result = timing_module._fixed_system_tls_context()

    assert result is context
    assert context.loaded == (str(compiled_ca), None)
    assert str(ambient_ca) not in context.loaded


def test_public_reader_is_anonymous_exact_and_bounded() -> None:
    url = f"{FIXED_API_ROOT}/issues/{_ISSUE}"
    response = _HTTPResponse(url=url, raw=b"{}")
    reader, opener = _reader_with_response(response)

    assert reader._read(url, accept="application/vnd.github+json") == b"{}"
    assert opener.request is not None
    assert opener.request.full_url == url
    assert opener.request.method == "GET"
    assert opener.request.get_header("Authorization") is None
    assert opener.timeout == 30
    assert response.read_limit == MAX_GITHUB_RESPONSE_BYTES + 1


def test_public_reader_rejects_redirect_status_size_and_duplicate_json() -> None:
    url = f"{FIXED_API_ROOT}/issues/{_ISSUE}"
    failures = (
        _HTTPResponse(url=f"{url}/redirected", raw=b"{}"),
        _HTTPResponse(url=url, raw=b"{}", status=206),
        _HTTPResponse(url=url, raw=b"x" * (MAX_GITHUB_RESPONSE_BYTES + 1)),
    )
    for response in failures:
        reader, _ = _reader_with_response(response)
        with pytest.raises(GitHubReadError):
            reader._read(url, accept="application/vnd.github+json")

    reader, _ = _reader_with_response(
        _HTTPResponse(url=url, raw=b'{"same":1,"same":2}')
    )
    with pytest.raises(GitHubReadError, match="non-strict JSON"):
        reader._json(url)
