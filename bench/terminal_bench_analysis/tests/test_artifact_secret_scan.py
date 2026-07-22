import base64
import gzip
from pathlib import Path

import pytest

import artifact_secret_scan
from artifact_secret_scan import environment_needles, main, scan_tree


def test_exact_and_encoded_environment_secret_are_blocked(tmp_path: Path) -> None:
    secret = "test-secret-value-123456789"
    (tmp_path / "plain.log").write_text(f"value={secret}")
    (tmp_path / "encoded.log").write_text("dGVzdC1zZWNyZXQtdmFsdWUtMTIzNDU2Nzg5")

    needles = environment_needles({"OPENROUTER_API_KEY": secret})
    findings, scanned = scan_tree(tmp_path, needles)

    assert scanned == 2
    assert {finding.path for finding in findings} == {"plain.log", "encoded.log"}
    assert all(secret not in finding.kind for finding in findings)


def test_provider_pattern_is_blocked_without_loaded_environment(tmp_path: Path) -> None:
    (tmp_path / "trajectory.json").write_text(
        '{"output":"sk-or-v1-abcdefghijklmnopqrstuvwxyz123456"}'
    )

    findings, _ = scan_tree(tmp_path, ())

    assert [(finding.path, finding.kind) for finding in findings] == [
        ("trajectory.json", "openrouter-token")
    ]


def test_clean_tree_passes(tmp_path: Path) -> None:
    (tmp_path / "result.json").write_text('{"reward": 1}')

    findings, scanned = scan_tree(tmp_path, ())

    assert findings == []
    assert scanned == 1


def test_required_environment_is_fail_closed(tmp_path: Path, monkeypatch) -> None:
    monkeypatch.delenv("OPENROUTER_API_KEY", raising=False)

    assert main([str(tmp_path), "--require-env", "OPENROUTER_API_KEY"]) == 2


def test_required_environment_is_also_scanned(tmp_path: Path, monkeypatch) -> None:
    secret = "harbor-private-value-123456"
    monkeypatch.setenv("HARBOR_AUTH", secret)
    (tmp_path / "log.txt").write_text(secret)

    assert main([str(tmp_path), "--require-env", "HARBOR_AUTH", "--json"]) == 1


@pytest.mark.parametrize(
    "encode",
    [
        lambda value: value.encode().hex(),
        lambda value: base64.b64encode(value.encode()).decode().rstrip("="),
        lambda value: "".join(f"\\u{byte:04x}" for byte in value.encode()),
        lambda value: value[::-1],
    ],
)
def test_additional_encoded_variants_are_blocked(tmp_path: Path, encode) -> None:
    secret = "encoded-secret-value-12345"
    (tmp_path / "encoded.txt").write_text(encode(secret))

    findings, _ = scan_tree(
        tmp_path,
        environment_needles({"OPENROUTER_API_KEY": secret}),
    )

    assert findings


def test_compressed_secret_is_blocked(tmp_path: Path) -> None:
    secret = "compressed-secret-value-12345"
    with gzip.open(tmp_path / "logs.json.gz", "wb") as handle:
        handle.write(secret.encode())

    findings, _ = scan_tree(
        tmp_path,
        environment_needles({"OPENROUTER_API_KEY": secret}),
    )

    assert any("gzip-content" in finding.path for finding in findings)


def test_secret_in_filename_is_blocked_without_echoing_it(tmp_path: Path) -> None:
    secret = "filename-secret-value-12345"
    (tmp_path / secret).write_text("clean")

    findings, _ = scan_tree(
        tmp_path,
        environment_needles({"OPENROUTER_API_KEY": secret}),
    )

    assert findings
    assert all(secret not in finding.path for finding in findings)
    assert any(finding.path.startswith("<redacted-path:") for finding in findings)


def test_unreadable_subtree_blocks_instead_of_skipping(
    tmp_path: Path, monkeypatch
) -> None:
    blocked = tmp_path / "blocked"
    blocked.mkdir()
    real_scandir = artifact_secret_scan.os.scandir

    def deny_blocked(path):
        if Path(path) == blocked:
            raise PermissionError("blocked for witness")
        return real_scandir(path)

    monkeypatch.setattr(artifact_secret_scan.os, "scandir", deny_blocked)

    with pytest.raises(PermissionError):
        scan_tree(tmp_path, ())


def test_symlink_blocks_publication(tmp_path: Path) -> None:
    target = tmp_path / "target.txt"
    target.write_text("clean")
    (tmp_path / "link.txt").symlink_to(target)

    findings, _ = scan_tree(tmp_path, ())

    assert any(finding.kind == "symlink-blocked" for finding in findings)
