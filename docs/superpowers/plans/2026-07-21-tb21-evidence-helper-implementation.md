# Terminal-Bench 2.1 evidence helper implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a deterministic local CLI that authors and validates hybrid-study evidence while remaining unable to publish, spend, launch, upload, or describe local checks as online authorization.

**Architecture:** A thin `stella-tb21-evidence` dispatcher loads the exact shared contract completed by the preceding hybrid-contract plan. Filesystem/Git mutation safety lives in a separate local I/O module. Runtime and OpenRouter snapshots are separate read-only entry points owned by the secure-launch boundary, so the local evidence CLI never reads a credential or opens an IP connection.

**Tech Stack:** Python 3.12/3.13 standard library, Harbor 0.6.1 read-only job parsing, pytest, Ruff, existing shared hybrid contract.

## Global Constraints

- Prerequisite: every completion-gate command in `docs/superpowers/plans/2026-07-21-tb21-hybrid-contract-implementation.md` passes from a clean tree.
- The helper is deterministic, credential-free, IP-network-free, and cannot invoke Git mutation, GitHub mutation, Harbor execution, cloud commands, uploads, submissions, or model APIs.
- The only permitted socket is exact local `unix:///var/run/docker.sock`, explicitly selected by every read-only Docker probe. Pulls, starts, execs, daemon mutation, ambient Docker contexts, and alternate socket paths are forbidden.
- Fixed tracked outputs are `bench/evidence/stella-tb21-task-partition.json`, `bench/evidence/stella-tb21-run-ledger.json`, `bench/evidence/stella-tb21-study-manifest.json`, and `bench/evidence/host-attestations/{intent_sha256}.json`.
- Tracked JSON is strict canonical UTF-8 with sorted keys, compact separators, finite numbers, and one trailing newline. GitHub bodies use the same JSON encoding without a trailing newline.
- Every tracked write is compare-and-swap, owner-only, symlink-safe, fsync-backed, atomic, and has no force mode.
- `github-comments.json` is not created until every required comment and the final ledger commit exist.
- `record-outcome` performs pattern scanning only. A separate publication gate compares the complete tree against the exact runtime and management credential values.
- `validate-local` always reports `launch_authorized:false`; exit zero means only local structural readiness.
- No production evidence initialization, GitHub comment, push, AWS resource, OpenRouter inference, Harbor launch, upload, or leaderboard submission occurs until the implementation has landed on public `main` and is reverified.
- Paid handoff is additionally blocked on a separate reviewed runner-provisioning plan and its verified execution; these two protocol/evidence plans do not authorize or implement AWS mutations.
- The completed package versions are `stella-terminal-bench-analysis==0.3.0` and `stella-harbor==0.8.0`. Every task that changes the shared contract or full analyzer updates the corresponding secure-launcher source-frozen digest and loader witness in the same commit.
- All commits use Conventional Commits and `git commit -s`.

---

## File structure

### Create

- `bench/harbor_adapter/stella_harbor/evidence_filesystem.py` — safe repository paths, strict reads, local Git reads, CAS, create-only persistence, and ledger/host transactions.
- `bench/harbor_adapter/stella_harbor/tb21_evidence.py` — thin argparse dispatcher and eight local command handlers.
- `bench/harbor_adapter/stella_harbor/runtime_snapshot.py` — credential-fingerprint/local-runtime snapshot exporter.
- `bench/harbor_adapter/stella_harbor/provider_snapshot.py` — exact three-GET OpenRouter control snapshot exporter.
- `bench/harbor_adapter/tests/test_evidence_filesystem.py` — path, CAS, atomicity, failpoint, and Git allowlist witnesses.
- `bench/harbor_adapter/tests/test_tb21_evidence.py` — command/golden/lifecycle/outcome/manifest/local-validation witnesses.
- `bench/harbor_adapter/tests/test_control_snapshots.py` — runtime/provider exporter and secret/network witnesses.
- `bench/harbor_adapter/tests/fixtures/tb21_hybrid_task_partition.golden.json`
- `bench/harbor_adapter/tests/fixtures/tb21_hybrid_initial_ledger.golden.json`
- `bench/harbor_adapter/tests/fixtures/tb21_preregistration_comment.golden.json`
- `bench/harbor_adapter/tests/fixtures/tb21_intent_comment.golden.json`
- `bench/harbor_adapter/tests/fixtures/tb21_github_comments_complete.golden.json`

### Modify

- `bench/terminal_bench_analysis/tb21_evidence_contract.py` — add immutable builders/appenders and snapshot/comment/local-result validators used by the CLI.
- `bench/terminal_bench_analysis/pyproject.toml` and `uv.lock` — bump the completed analysis package to `0.3.0` and lock it.
- `bench/terminal_bench_analysis/tb21_analysis.py` — retain the read-only `derive_completed_job_evidence` API from the prerequisite plan.
- `bench/terminal_bench_analysis/artifact_secret_scan.py` and tests — recognize both approved management-key environment names in the separate exact-value gate.
- `bench/harbor_adapter/stella_harbor/credential_bundle.py` and tests — canonicalize exactly one management-key alias and scrub both names/values.
- `bench/harbor_adapter/stella_harbor/secure_launcher.py` and tests — reuse snapshot validation/client code, bind helper/exporter digests, and include new modules in fixed public source verification.
- `bench/harbor_adapter/stella_harbor/contract_loader.py` — update the source-frozen shared-contract digest whenever the contract changes.
- `bench/harbor_adapter/stella_harbor/host_attestation.py` and tests — require the explicit local Docker Unix socket in collection and live recheck.
- `bench/harbor_adapter/pyproject.toml` and `uv.lock` — add three console entry points, package the new modules, and bump the completed adapter package to `0.8.0`.
- `bench/terminal-bench-2.1-protocol.md`, `bench/harbor_adapter/README.md`, `bench/terminal_bench_analysis/README.md`, and `bench/README.md` — document exact commands and authority boundaries.
- `Makefile` and `.github/workflows/ci.yml` — include the new focused suites in `bench-gate`.

---

### Task 1: Build the safe filesystem, local Git, and CLI skeleton

**Files:**
- Create: `bench/harbor_adapter/stella_harbor/evidence_filesystem.py`
- Create: `bench/harbor_adapter/stella_harbor/tb21_evidence.py`
- Create: `bench/harbor_adapter/tests/test_evidence_filesystem.py`
- Modify: `bench/harbor_adapter/stella_harbor/secure_launcher.py`
- Modify: `bench/harbor_adapter/tests/test_secure_launcher.py`
- Modify: `bench/harbor_adapter/pyproject.toml`
- Modify: `bench/harbor_adapter/uv.lock`

**Interfaces:**
- Produces: `RepositoryFilesystem`, `LocalGitReader.read_blob(commit, path)`, `LocalGitReader.is_strict_ancestor(older, newer)`, `atomic_compare_and_swap`, `atomic_create_idempotent`, and `commit_ledger_and_host_report`.
- Produces CLI exit contract: 0 success/local-ready, 1 contract failure, 2 usage or I/O failure.

- [ ] **Step 1: Write failing CAS, symlink, Git, and no-force witnesses**

```python
def test_atomic_compare_and_swap_is_owner_only_and_digest_guarded(tmp_path: Path) -> None:
    path = tmp_path / "ledger.json"
    path.write_bytes(b'{"old":true}\n')
    old = hashlib.sha256(path.read_bytes()).hexdigest()
    atomic_compare_and_swap(path, expected_sha256=old, new_bytes=b'{"new":true}\n')
    assert path.read_bytes() == b'{"new":true}\n'
    assert stat.S_IMODE(path.stat().st_mode) == 0o600
    with pytest.raises(EvidenceFilesystemError, match="preimage"):
        atomic_compare_and_swap(path, expected_sha256=old, new_bytes=b'{"again":true}\n')

def test_local_git_reader_allows_only_read_operations(git_repo: Path) -> None:
    reader = LocalGitReader(git_repo)
    assert reader.read_blob(HEAD, "ledger.json") == b'{"schema_version":"v"}\n'
    assert reader.is_strict_ancestor(PARENT, HEAD) is True
    assert reader.executed_verbs == ["cat-file", "show", "merge-base"]
```

The required parameterized test matrix is exactly: symlinked parents/files, wrong owner, noncanonical root, mode drift, short/zero `os.write`, destination inode/content swaps while waiting for the repository lock, revalidation failure immediately before replacement, injected failures before/after file replacement, directory fsync, identical create-only retry, conflicting create-only bytes, and ordinary rollback that may leave only an identical unreferenced host report.

- [ ] **Step 2: Run tests and confirm missing module failure**

Run: `cd bench/harbor_adapter && uv run --frozen --extra dev pytest -q tests/test_evidence_filesystem.py`

Expected: collection fails because `evidence_filesystem` does not exist.

- [ ] **Step 3: Implement safe persistence primitives**

```python
def write_all(fd: int, data: bytes) -> None:
    view = memoryview(data)
    while view:
        written = os.write(fd, view)
        if written <= 0:
            raise EvidenceFilesystemError("short write made no progress")
        view = view[written:]

def atomic_compare_and_swap(path: Path, *, expected_sha256: str, new_bytes: bytes) -> None:
    with repository_write_lock(path) as repository_fd:
        directory_fd, destination_name = open_canonical_parent_at(repository_fd, path)
        with open_regular_nofollow_at(directory_fd, destination_name) as original_fd:
            original_identity = file_identity(original_fd)
            if sha256_fd(original_fd) != expected_sha256:
                raise EvidenceFilesystemError("preimage digest mismatch")
        temporary_fd, temporary_name = create_mode_0600_temp_at(directory_fd)
        try:
            write_all(temporary_fd, new_bytes)
            os.fsync(temporary_fd)
            if sha256_fd(temporary_fd) != hashlib.sha256(new_bytes).hexdigest():
                raise EvidenceFilesystemError("temporary file digest mismatch")
            with open_regular_nofollow_at(directory_fd, destination_name) as current_fd:
                if (file_identity(current_fd) != original_identity
                        or sha256_fd(current_fd) != expected_sha256):
                    raise EvidenceFilesystemError("preimage changed before replacement")
            os.replace(temporary_name, destination_name,
                       src_dir_fd=directory_fd, dst_dir_fd=directory_fd)
            fsync_directory_fd(directory_fd)
        finally:
            close_and_unlink_temp_at(directory_fd, temporary_fd, temporary_name)
```

`repository_write_lock` takes an exclusive `flock` on the already-open canonical repository-root directory before any preimage read; every helper mutation uses that same lock. All path traversal and replacement uses held directory FDs and no-follow opens, and the destination inode plus digest are revalidated under the lock immediately before `os.replace`. This provides conditional replacement among helper processes and detects external swaps at the final revalidation point. Open every existing component with no-follow semantics before use. Git subprocesses set `GIT_OPTIONAL_LOCKS=0`, clear hooks/config injection, and allow only `cat-file`, `show`, and `merge-base --is-ancestor`.

- [ ] **Step 4: Add the thin parser without command behavior**

```python
def main(argv: Sequence[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    try:
        return int(args.handler(args))
    except ContractError as error:
        print(f"{error.category}: {error}", file=sys.stderr)
        return 1
    except (OSError, EvidenceFilesystemError) as error:
        print(f"filesystem: {error}", file=sys.stderr)
        return 2
```

Register `stella-tb21-evidence = "stella_harbor.tb21_evidence:main"`. Every subcommand exists in help but raises a deterministic `usage` error until its task is implemented.

Add `evidence_filesystem.py` and `tb21_evidence.py` to the launcher's exact adapter-source allowlist as soon as the files exist, add a recursive-source witness that fails if either is absent, and bump/lock the adapter package at `0.8.0`.

- [ ] **Step 5: Verify and commit the local I/O boundary**

```bash
cd bench/harbor_adapter
uv run --frozen --extra dev pytest -q \
  tests/test_evidence_filesystem.py tests/test_secure_launcher.py
uv run --frozen --extra dev stella-tb21-evidence --help
cd ../..
git add bench/harbor_adapter/stella_harbor/evidence_filesystem.py \
  bench/harbor_adapter/stella_harbor/tb21_evidence.py \
  bench/harbor_adapter/stella_harbor/secure_launcher.py \
  bench/harbor_adapter/tests/test_evidence_filesystem.py \
  bench/harbor_adapter/tests/test_secure_launcher.py \
  bench/harbor_adapter/pyproject.toml bench/harbor_adapter/uv.lock
git commit -s -m "feat(bench): add safe local evidence filesystem"
```

Expected: tests pass and help lists all eight commands without reading environment credentials.

### Task 2: Implement `init-study` and golden initial evidence

**Files:**
- Modify: `bench/terminal_bench_analysis/tb21_evidence_contract.py`
- Modify: `bench/terminal_bench_analysis/pyproject.toml`
- Modify: `bench/terminal_bench_analysis/uv.lock`
- Modify: `bench/harbor_adapter/stella_harbor/contract_loader.py`
- Modify: `bench/harbor_adapter/stella_harbor/tb21_evidence.py`
- Modify: `bench/harbor_adapter/tests/test_secure_launcher.py`
- Create: `bench/harbor_adapter/tests/test_tb21_evidence.py`
- Create: `bench/harbor_adapter/tests/fixtures/tb21_hybrid_task_partition.golden.json`
- Create: `bench/harbor_adapter/tests/fixtures/tb21_hybrid_initial_ledger.golden.json`

**Interfaces:**
- Consumes: the fixed source seed, verified comparator directory, locally materialized pinned dataset directory, repository root, and fixed evidence paths.
- Consumes an explicit timezone-aware authorization declaration and the local Git commit whose approved study design states the exact $100 provider/$55 infrastructure scope; it never infers approval time from the clock or chat history.
- Produces: exactly one partition file and one initial ledger file plus a noncanonical stdout receipt containing only paths and SHA-256 digests.

- [ ] **Step 1: Write the failing initialization witness**

```python
def test_init_study_writes_exact_golden_bytes(tmp_repository: Path, capsys: pytest.CaptureFixture[str]) -> None:
    result = main(["init-study", "--repository", str(tmp_repository),
                   "--comparator-dir", str(PINNED_COMPARATOR_FIXTURE),
                   "--dataset-dir", str(PINNED_DATASET_FIXTURE),
                   "--authorization-commit", APPROVED_DESIGN_COMMIT,
                   "--declared-at", "2026-07-21T12:00:00-07:00"])
    assert result == 0
    assert (tmp_repository / TASK_PARTITION_PATH).read_bytes() == PARTITION_GOLDEN.read_bytes()
    assert (tmp_repository / RUN_LEDGER_PATH).read_bytes() == LEDGER_GOLDEN.read_bytes()
    assert json.loads(capsys.readouterr().out) == {
        "ledger_path": RUN_LEDGER_PATH,
        "ledger_sha256": hashlib.sha256(LEDGER_GOLDEN.read_bytes()).hexdigest(),
        "task_partition_path": TASK_PARTITION_PATH,
        "task_partition_sha256": hashlib.sha256(PARTITION_GOLDEN.read_bytes()).hexdigest(),
    }
```

The required parameterized rejection matrix is exactly: wrong manifest/submission/audit SHA, wrong comparator job ID, missing/extra trial files, local dataset-file mutation that changes a Harbor task checksum, wrong 89-task identity, missing/nonancestor design commit, design bytes without approved exact caps, naive declaration time, existing output, ambient historical files, nonempty output directories, and a second invocation.

- [ ] **Step 2: Run the witness and verify the command is unimplemented**

Run: `cd bench/harbor_adapter && uv run --frozen --extra dev pytest -q tests/test_tb21_evidence.py tests/test_secure_launcher.py -k 'init_study or frozen_analysis_modules'`

Expected: exit 2 or the deterministic unimplemented usage error.

- [ ] **Step 3: Implement immutable construction in the pure contract**

```python
def build_initial_ledger(partition_sha256: str, *, authorization_commit: str,
                         declared_at: str) -> dict[str, object]:
    return {
        "schema_version": RUN_LEDGER_SCHEMA,
        "study_id": STUDY_ID,
        "paths": FIXED_PATHS,
        "task_partition_sha256": partition_sha256,
        "budget_authorizations": initial_budget_authorizations(
            authorization_commit=authorization_commit, declared_at=declared_at
        ),
        "prior_exploration_disclosure": prior_exploration_disclosure(),
        "preregistrations": [], "candidates": [], "intents": [],
        "publications": [], "outcomes": [],
    }
```

Initial budget records encode only the $100 provider and $55 AWS tuning/screen authorizations, bind the explicit approved-design commit/declaration, and contain no confirmatory authorization. The reviewed prior-exploration constant names exactly `9b704487-9d21-46a7-8103-e5396cb7d4ea`, `0c44d9ee-4389-4c7a-8445-ea4be2404115`, `c5686c41-1d2d-41cf-a275-177c2e6878b3`, `37ee4276-8595-4ff9-8507-be21adb891cc`, and `7e59ed1e-2abe-40b9-bf7e-6b24c7f9a350` as excluded historical jobs and states that no v6 paid readiness, calibration, or primary call is eligible for the hybrid study. It is frozen source, not reconstructed from ambient job directories.

Bump/lock the analysis package at `0.3.0`, update the launcher's source-frozen contract digest after the final contract bytes are formatted, and run the loader drift witness before committing.

- [ ] **Step 4: Implement create-only command persistence**

Build both byte strings completely, validate them again, then create both fixed files mode 0600. If the second create fails, remove only the first file created by the same invocation after verifying its inode and bytes; never remove a pre-existing file.

Run: `cd bench/harbor_adapter && uv run --frozen --extra dev pytest -q tests/test_tb21_evidence.py tests/test_secure_launcher.py -k 'init_study or frozen_analysis_modules'`

Expected: all initialization tests pass.

- [ ] **Step 5: Commit initialization and goldens**

```bash
git add bench/terminal_bench_analysis/tb21_evidence_contract.py \
  bench/terminal_bench_analysis/pyproject.toml \
  bench/terminal_bench_analysis/uv.lock \
  bench/harbor_adapter/stella_harbor/contract_loader.py \
  bench/harbor_adapter/stella_harbor/tb21_evidence.py \
  bench/harbor_adapter/tests/test_tb21_evidence.py \
  bench/harbor_adapter/tests/test_secure_launcher.py \
  bench/harbor_adapter/tests/fixtures/tb21_hybrid_task_partition.golden.json \
  bench/harbor_adapter/tests/fixtures/tb21_hybrid_initial_ledger.golden.json
git commit -s -m "feat(bench): initialize hybrid evidence"
```

### Task 3: Implement preregistration, comment rendering, and publication recording

**Files:**
- Modify: `bench/terminal_bench_analysis/tb21_evidence_contract.py`
- Modify: `bench/harbor_adapter/stella_harbor/contract_loader.py`
- Modify: `bench/harbor_adapter/stella_harbor/tb21_evidence.py`
- Modify: `bench/harbor_adapter/tests/test_tb21_evidence.py`
- Modify: `bench/harbor_adapter/tests/test_secure_launcher.py`
- Create: `bench/harbor_adapter/tests/fixtures/tb21_preregistration_comment.golden.json`
- Create: `bench/harbor_adapter/tests/fixtures/tb21_intent_comment.golden.json`

**Interfaces:**
- Produces: `append-preregistration`, `render-comment`, and `record-publication`.
- `append-preregistration` accepts `--kind`, `--subject-commit`, `--declared-at`, `--artifact`, repeatable `--candidate`, and `--expected-ledger-sha256`.
- A screen or confirmatory preregistration binds an already committed phase manifest and its digest; this task never constructs or changes that manifest. `freeze-manifest` is implemented in Task 5 and must run first for those two phases.
- `development_amendment` is the only replacement form: it binds the completed ineligible outcome, preserved artifact-tree digest, reason, replacement intent metadata, and remaining round/budget capacity before that replacement intent can be prepared.

- [ ] **Step 1: Write lifecycle/comment/publication witnesses**

```python
def test_render_comment_is_exact_and_has_no_newline(committed_ledger: Path, capsysbinary: pytest.CaptureFixture[bytes]) -> None:
    assert main(["render-comment", "--repository", str(REPOSITORY),
                 "--subject-type", "preregistration", "--subject-id", "development_round_1",
                 "--subject-commit", SOURCE_COMMIT, "--ledger-commit", LEDGER_COMMIT]) == 0
    assert capsysbinary.readouterr().out == PREREG_COMMENT_GOLDEN.read_bytes()
    assert not PREREG_COMMENT_GOLDEN.read_bytes().endswith(b"\n")

def test_record_publication_uses_commit_url_not_comment_url() -> None:
    ledger = record_publication(LEDGER, COMMENT_EXPORT, ISSUE_EXPORT)
    publication = ledger["publications"][-1]
    assert publication["public_url"] == f"https://github.com/macanderson/stella/commit/{LEDGER_COMMIT}"
    assert publication["published_at"] == COMMENT_EXPORT["created_at"]
```

Cover candidate append with dev round preregistration, deterministic promotion, development replacement without a published amendment, amendment after stage-cap exhaustion, rejection of a missing or postdated screen/confirmatory manifest, exact pre-existing manifest and pre-existing confirmatory authorization binding, attempts to add/change budget inside preregistration, strict ancestry, exact committed blob, `created_at == updated_at`, owner association, wrong issue/repository, edited body, duplicate publication, and snapshot drift.

- [ ] **Step 2: Run focused tests and confirm red state**

Run: `cd bench/harbor_adapter && uv run --frozen --extra dev pytest -q tests/test_tb21_evidence.py tests/test_secure_launcher.py -k 'preregistration or render_comment or publication or frozen_analysis_modules'`

Expected: command handlers return the unimplemented usage error.

- [ ] **Step 3: Implement pure append and comment builders**

```python
def render_github_attestation(*, subject_type: str, subject_id: str,
                              kind: str, subject_commit: str,
                              ledger_commit: str, ledger_sha256: str) -> bytes:
    body = {
        "schema_version": GITHUB_ATTESTATION_SCHEMA, "study_id": STUDY_ID,
        "subject_type": subject_type, "subject_id": subject_id, "kind": kind,
        "subject_commit": subject_commit, "ledger_commit": ledger_commit,
        "ledger_path": RUN_LEDGER_PATH, "ledger_sha256": ledger_sha256,
    }
    return canonical_body_bytes(body)
```

`append_preregistration` validates the legal lifecycle prefix before allocating the next global sequence. A development preregistration atomically adds its frozen candidate records. A confirmatory preregistration can only bind the already committed confirmatory manifest and the exact authorization previously appended by the confirmatory `freeze-manifest` transition; it cannot add or alter funding.

After formatting the shared contract, update and test the loader's source-frozen digest before the commit.

- [ ] **Step 4: Implement Git-object and paired-export command checks**

`render-comment` reads the committed ledger blob, requires strict subject-commit ancestry, and writes with `sys.stdout.buffer.write`. `record-publication` validates both the GitHub comment and issue exports locally, appends exactly one publication, returns the comment URL only in its local receipt, and never creates a partial comment map.

Run: `cd bench/harbor_adapter && uv run --frozen --extra dev pytest -q tests/test_tb21_evidence.py tests/test_secure_launcher.py -k 'preregistration or render_comment or publication or frozen_analysis_modules'`

Expected: all focused tests pass.

- [ ] **Step 5: Commit lifecycle authoring**

```bash
git add bench/terminal_bench_analysis/tb21_evidence_contract.py \
  bench/harbor_adapter/stella_harbor/contract_loader.py \
  bench/harbor_adapter/stella_harbor/tb21_evidence.py \
  bench/harbor_adapter/tests/test_tb21_evidence.py \
  bench/harbor_adapter/tests/test_secure_launcher.py \
  bench/harbor_adapter/tests/fixtures/tb21_preregistration_comment.golden.json \
  bench/harbor_adapter/tests/fixtures/tb21_intent_comment.golden.json
git commit -s -m "feat(bench): author public evidence lifecycle"
```

### Task 4: Add isolated runtime and provider snapshot exporters

**Files:**
- Create: `bench/harbor_adapter/stella_harbor/runtime_snapshot.py`
- Create: `bench/harbor_adapter/stella_harbor/provider_snapshot.py`
- Create: `bench/harbor_adapter/tests/test_control_snapshots.py`
- Modify: `bench/harbor_adapter/stella_harbor/credential_bundle.py`
- Modify: `bench/harbor_adapter/stella_harbor/secure_launcher.py`
- Modify: `bench/harbor_adapter/tests/test_secure_launcher.py`
- Modify: `bench/harbor_adapter/pyproject.toml`
- Modify: `bench/harbor_adapter/uv.lock`

**Interfaces:**
- Produces scripts `stella-tb21-runtime-snapshot` and `stella-tb21-provider-snapshot`.
- Produces `management_credential_from_environment(environ) -> tuple[str, str]`, where the first value is the canonical source name and the second is the credential.

- [ ] **Step 1: Write credential and socket tripwire witnesses**

```python
def test_management_alias_requires_exactly_one_name() -> None:
    assert management_credential_from_environment({"OPENROUTER_MGMT_KEY": "m"}) == ("OPENROUTER_MGMT_KEY", "m")
    with pytest.raises(RuntimeError, match="exactly one"):
        management_credential_from_environment({
            "OPENROUTER_MGMT_KEY": "m", "OPENROUTER_MANAGEMENT_API_KEY": "m",
        })

def test_provider_snapshot_uses_exact_three_gets(fake_reader: FakeReader) -> None:
    snapshot = build_provider_snapshot(ENVIRONMENT, reader=fake_reader)
    assert fake_reader.calls == [
        ("runtime", "https://openrouter.ai/api/v1/key"),
        ("management", f"https://openrouter.ai/api/v1/keys/{RUNTIME_FINGERPRINT}"),
        ("management", "https://openrouter.ai/api/v1/credits"),
    ]
    assert "runtime-secret" not in canonical_file_bytes(snapshot).decode()
    assert "management-secret" not in canonical_file_bytes(snapshot).decode()
```

Runtime tests fail on management-key access or any socket. Provider tests fail on a fourth request, redirect, proxy, alternate origin, missing/invalid server `Date`, response-shape drift, key hash/name/limit/reset/BYOK/disabled mismatch, insufficient limit/credit, or any secret byte in output/error text.

- [ ] **Step 2: Run snapshot witnesses and confirm missing modules**

Run: `cd bench/harbor_adapter && uv run --frozen --extra dev pytest -q tests/test_control_snapshots.py`

Expected: collection fails because the exporter modules do not exist.

- [ ] **Step 3: Implement the runtime exporter**

```python
def build_runtime_snapshot(command: Sequence[str], environ: Mapping[str, str]) -> dict[str, object]:
    runtime_key = require_runtime_key(environ)
    identity, python_executable = validated_runtime_identity(command, environ, {"OPENROUTER_API_KEY": runtime_key})
    return validate_runtime_snapshot({
        "schema_version": "stella-tb21-runtime-snapshot-v1",
        "producer_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        "runtime_identity": identity,
        "python_sha256": sha256_file(python_executable),
    })
```

It reads only `OPENROUTER_API_KEY` to hash its fingerprint, performs local runtime/source checks, writes one canonical create-only file, and never opens a socket.

- [ ] **Step 4: Implement the provider exporter and share validation with launcher**

The reader disables redirects and ambient proxies, fixes TLS origin/path/method, sends the runtime key only to `/key`, sends the management key only to `/keys/{runtime_key_sha256}` and `/credits`, and parses server `Date` headers. JSON monetary tokens are parsed directly with `Decimal`, validated as finite/nonnegative/bounded, and emitted as normalized decimal strings; no provider or telemetry amount passes through binary float. The canonical snapshot records three server dates, live usage/remaining, key name/limit/reset/BYOK/disabled posture, total credits/usage/available, response payload digests, and producer digest.

Register both scripts, update `_FIXED_ADAPTER_SOURCE_PATHS`, and make the secure launcher call the same response validator rather than retaining a second implementation.

- [ ] **Step 5: Run full snapshot/launcher checks and commit**

```bash
cd bench/harbor_adapter
uv run --frozen --extra dev pytest -q tests/test_control_snapshots.py tests/test_secure_launcher.py
cd ../..
git add bench/harbor_adapter/stella_harbor/runtime_snapshot.py \
  bench/harbor_adapter/stella_harbor/provider_snapshot.py \
  bench/harbor_adapter/stella_harbor/credential_bundle.py \
  bench/harbor_adapter/stella_harbor/secure_launcher.py \
  bench/harbor_adapter/tests/test_control_snapshots.py \
  bench/harbor_adapter/tests/test_secure_launcher.py \
  bench/harbor_adapter/pyproject.toml bench/harbor_adapter/uv.lock
git commit -s -m "feat(bench): export read-only control snapshots"
```

Expected: all tests pass without real network or credentials.

### Task 5: Implement host-bound intents and phase manifests

**Files:**
- Modify: `bench/terminal_bench_analysis/tb21_evidence_contract.py`
- Modify: `bench/harbor_adapter/stella_harbor/contract_loader.py`
- Modify: `bench/harbor_adapter/stella_harbor/evidence_filesystem.py`
- Modify: `bench/harbor_adapter/stella_harbor/tb21_evidence.py`
- Modify: `bench/harbor_adapter/stella_harbor/host_attestation.py`
- Modify: `bench/harbor_adapter/tests/test_evidence_filesystem.py`
- Modify: `bench/harbor_adapter/tests/test_tb21_evidence.py`
- Modify: `bench/harbor_adapter/tests/test_host_attestation.py`
- Modify: `bench/harbor_adapter/tests/test_secure_launcher.py`

**Interfaces:**
- Produces: `prepare-intent`, `freeze-manifest`, and `commit_ledger_and_manifest`.
- `prepare-intent` consumes candidate ID, exact Harbor argv JSON, subject commit, runtime/provider snapshots, jobs directory, Docker executable/socket, explicit declaration time, and expected ledger digest.
- Confirmatory `freeze-manifest` consumes a separately reviewed authorization artifact, explicit authorization time, expected ledger preimage digest, expected screen-manifest preimage digest, and the distinct confirmatory runtime identity; it atomically appends the authorization and replaces the phase manifest.

- [ ] **Step 1: Write intent transaction and manifest-order witnesses**

```python
def test_prepare_intent_writes_bound_ledger_and_host_report(tmp_repository: Path) -> None:
    receipt = run_prepare_intent(tmp_repository, stage="screen")
    ledger = load_ledger(tmp_repository)
    intent = ledger["intents"][-1]
    report = tmp_repository / f"bench/evidence/host-attestations/{intent['intent_sha256']}.json"
    assert report.is_file()
    assert json.loads(report.read_bytes())["intent_sha256"] == intent["intent_sha256"]
    assert receipt["launch_authorized"] is False

def test_confirmatory_manifest_atomically_appends_new_authorization(tmp_repository: Path) -> None:
    before = load_ledger(tmp_repository)
    receipt = run_freeze_manifest(tmp_repository, phase="confirmatory",
                                  budget_authorization=NEW_AUTHORIZATION)
    after = load_ledger(tmp_repository)
    assert len(after["budget_authorizations"]) == len(before["budget_authorizations"]) + 1
    assert after["budget_authorizations"][-1] == NEW_AUTHORIZATION
    assert receipt["manifest_phase"] == "confirmatory"

def test_confirmatory_manifest_requires_explicit_new_authorization() -> None:
    with pytest.raises(ContractError, match="confirmatory authorization"):
        freeze_manifest_transition(ledger=screen_passed_ledger_without_new_budget(),
                                   phase="confirmatory", budget_authorization=None)
```

Add stage/model/task/trial/concurrency/retry/budget drift, stale snapshot, wrong producer digest, Docker remote context, ledger/report failpoints, orphan identical report retry, screen manifest before dev gate, confirmatory manifest before screen pass, authorization without explicit owner fields, authorization or manifest partial-write failpoints, and both ledger/manifest preimage mismatches.

- [ ] **Step 2: Run focused tests and confirm red state**

Run: `cd bench/harbor_adapter && uv run --frozen --extra dev pytest -q tests/test_tb21_evidence.py tests/test_evidence_filesystem.py -k 'intent or manifest or host'`

Expected: unimplemented command/contract failures.

- [ ] **Step 3: Implement intent construction and two-file transaction**

The intent digest is computed before host collection from immutable candidate, argv, runtime/provider snapshots, authorization, and declaration time. The host report then binds that digest. Stage shape comes only from the shared contract. `commit_ledger_and_host_report` create-links the host report, atomically replaces the ledger, and leaves at most an identical unreferenced report on process death. Add the same locked/revalidated transaction discipline to `commit_ledger_and_manifest`.

- [ ] **Step 4: Implement phase-specific manifest freezing**

```python
def freeze_manifest_transition(*, phase: str, ledger: Mapping[str, object],
                               partition: Mapping[str, object], candidate_id: str,
                               runtime_snapshot: Mapping[str, object],
                               budget_authorization: Mapping[str, object] | None
                               ) -> tuple[dict[str, object], dict[str, object]]:
    if phase not in {"screen", "confirmatory"}:
        raise ContractError("contract", "manifest phase is not registered")
    updated = (
        append_confirmatory_authorization(ledger, budget_authorization)
        if phase == "confirmatory"
        else require_no_authorization(ledger, budget_authorization)
    )
    validate_manifest_prerequisites(phase, updated, candidate_id)
    manifest = derive_manifest_from_immutable_inputs(
        phase, updated, partition, candidate_id, runtime_snapshot
    )
    return updated, manifest
```

Screen creation is create-only. Confirmatory creation performs one logical ledger/manifest transaction: fully stage both outputs, replace the ledger first with the new finite provider/infrastructure authorization, then compare-and-swap the committed screen manifest for the bound confirmatory manifest under the same repository lock. Ordinary failure restores both preimages. A crash between replacements leaves a recognizable new-authorization/old-screen-manifest prefix in which preregistration and launch remain forbidden; an identical retry may finish the manifest replacement, while any differing retry fails. Git history preserves the screen bytes. No unknown ID, manual metric, or override is accepted.

The screen manifest requires a development winner that passed the eligibility gate but does not require a screen preregistration; the next lifecycle action appends that preregistration bound to the manifest's committed bytes. The confirmatory manifest similarly precedes and is then bound by the confirmatory preregistration, after the passing screen and separate new authorization.

After formatting the shared contract, update and test the loader's source-frozen digest before the commit.

- [ ] **Step 5: Verify and commit intent/manifest authoring**

```bash
cd bench/harbor_adapter
uv run --frozen --extra dev pytest -q tests/test_tb21_evidence.py \
  tests/test_evidence_filesystem.py tests/test_host_attestation.py \
  tests/test_secure_launcher.py
cd ../..
git add bench/terminal_bench_analysis/tb21_evidence_contract.py \
  bench/harbor_adapter/stella_harbor/contract_loader.py \
  bench/harbor_adapter/stella_harbor/evidence_filesystem.py \
  bench/harbor_adapter/stella_harbor/tb21_evidence.py \
  bench/harbor_adapter/stella_harbor/host_attestation.py \
  bench/harbor_adapter/tests/test_evidence_filesystem.py \
  bench/harbor_adapter/tests/test_tb21_evidence.py \
  bench/harbor_adapter/tests/test_host_attestation.py \
  bench/harbor_adapter/tests/test_secure_launcher.py
git commit -s -m "feat(bench): prepare host-bound paid intents"
```

### Task 6: Derive and record immutable job outcomes

**Files:**
- Modify: `bench/terminal_bench_analysis/tb21_analysis.py`
- Modify: `bench/terminal_bench_analysis/tb21_evidence_contract.py`
- Modify: `bench/harbor_adapter/stella_harbor/contract_loader.py`
- Modify: `bench/terminal_bench_analysis/artifact_secret_scan.py`
- Modify: `bench/terminal_bench_analysis/tests/test_artifact_secret_scan.py`
- Modify: `bench/harbor_adapter/stella_harbor/tb21_evidence.py`
- Modify: `bench/harbor_adapter/tests/test_tb21_evidence.py`
- Modify: `bench/harbor_adapter/tests/test_secure_launcher.py`

**Interfaces:**
- Produces: `record-outcome`, using analyzer-derived job evidence and before/after nonsecret provider snapshots.
- Consumes: `derive_completed_job_evidence(job_dir: Path, *, expected_intent: Mapping[str, object]) -> dict[str, object]`, `derive_stage_result(job_evidence, *, stage, partition, comparator_rows)`, the exact pinned comparator/partition, and `scan_tree(job_dir, ())` only.
- CLI inputs are the immutable job directory, pinned comparator directory, fixed task-partition file, before/after provider snapshot files, explicit timezone-aware `recorded_at`, and expected ledger preimage digest; pass/fail metrics and reconciled cost are never accepted as operator inputs.

- [ ] **Step 1: Write complete/failure/no-mutation witnesses**

```python
@pytest.mark.parametrize("mutation", [
    "missing_trial", "resumed_job", "stitched_job",
    "telemetry_incomplete", "intent_mismatch",
])
def test_record_outcome_refuses_ineligible_sealed_job(mutation: str, tmp_repository: Path) -> None:
    job = hybrid_job(mutation, stage="screen")
    before = snapshot_tree(job)
    assert run_record_outcome(tmp_repository, job) == 1
    assert snapshot_tree(job) == before

def test_exploratory_failure_is_appended_not_erased(tmp_repository: Path) -> None:
    assert run_record_outcome(tmp_repository, failed_dev_job()) == 0
    assert load_ledger(tmp_repository)["outcomes"][-1]["status"] == "failed"

def test_retried_job_is_preserved_but_ineligible(tmp_repository: Path) -> None:
    assert run_record_outcome(tmp_repository, retried_dev_job()) == 0
    outcome = load_ledger(tmp_repository)["outcomes"][-1]
    assert outcome["status"] == "ineligible"
    assert outcome["stage_result"]["artifact_eligible"] is False

def test_complete_but_statistically_failed_screen_is_recorded_and_blocks_confirmatory(tmp_repository: Path) -> None:
    assert run_record_outcome(tmp_repository, complete_losing_screen_job()) == 0
    outcome = load_ledger(tmp_repository)["outcomes"][-1]
    assert outcome["status"] == "complete"
    assert outcome["stage_result"]["artifact_eligible"] is True
    assert outcome["stage_result"]["gate_passed"] is False
    with pytest.raises(ContractError, match="screen gate"):
        run_freeze_manifest(tmp_repository, phase="confirmatory",
                            budget_authorization=NEW_AUTHORIZATION)
```

- [ ] **Step 2: Run outcome tests and confirm red state**

Run: `cd bench/harbor_adapter && uv run --frozen --extra dev pytest -q tests/test_tb21_evidence.py -k outcome`

Expected: `record-outcome` is unimplemented.

- [ ] **Step 3: Implement read-only derivation and reconciliation**

```python
job_evidence = analyzer.derive_completed_job_evidence(job_dir, expected_intent=intent)
stage_result = analyzer.derive_stage_result(
    job_evidence, stage=intent["stage"], partition=partition,
    comparator_rows=comparator_rows,
)
pattern_findings = scanner.scan_tree(job_dir, ())
outcome = contract.build_outcome(
    intent=intent, job_evidence=job_evidence, stage_result=stage_result,
    provider_before=before_snapshot, provider_after=after_snapshot,
    pattern_findings=pattern_findings, recorded_at=args.recorded_at,
)
```

The analyzer derives IDs, timestamps, artifact/trial digests, slot/failure/retry counts, every successful/failed/aborted paid-call ID and envelope, accounting completeness, telemetry cost, exact verifier/token totals, sampler/stream digests, rational improvements, and the applicable development/screen/confirmatory gate. The outcome binds the comparator manifest digest, partition digest, analyzer/contract digests, analysis-input digest, and canonical `stage_result`; pass/fail metrics are never operator inputs. The contract derives exact-decimal provider delta and reconciliation, rejects any missing call ID, and caps absolute reconciliation tolerance at the frozen one cent. A terminal execution failure or retry-bearing job with complete immutable evidence is retained as ineligible and cannot advance. A complete but statistically losing screen is retained with `gate_passed:false` and permanently blocks confirmatory preparation for this study.

After formatting the shared contract and analyzer, update and test both source-frozen digests before the commit.

- [ ] **Step 4: Strengthen the separate exact-secret gate**

Recognize `OPENROUTER_API_KEY`, `OPENROUTER_MGMT_KEY`, and `OPENROUTER_MANAGEMENT_API_KEY`. Exact-value publication tests set exactly one management alias and prove both runtime and management values are detected. The helper path passes an empty needle tuple and tests prove it never calls `environment_needles()`.

- [ ] **Step 5: Verify and commit outcome recording**

```bash
cd bench/terminal_bench_analysis
uv run --frozen --extra dev pytest -q tests/test_artifact_secret_scan.py tests/test_tb21_analysis.py
cd ../harbor_adapter
uv run --frozen --extra dev pytest -q \
  tests/test_tb21_evidence.py tests/test_secure_launcher.py \
  -k 'outcome or frozen_analysis_modules'
cd ../..
git add bench/terminal_bench_analysis/tb21_analysis.py \
  bench/terminal_bench_analysis/tb21_evidence_contract.py \
  bench/terminal_bench_analysis/artifact_secret_scan.py \
  bench/terminal_bench_analysis/tests/test_artifact_secret_scan.py \
  bench/harbor_adapter/stella_harbor/contract_loader.py \
  bench/harbor_adapter/stella_harbor/tb21_evidence.py \
  bench/harbor_adapter/tests/test_tb21_evidence.py \
  bench/harbor_adapter/tests/test_secure_launcher.py
git commit -s -m "feat(bench): record immutable benchmark outcomes"
```

### Task 7: Implement `validate-local`, boundary tripwires, and production round trip

**Files:**
- Modify: `bench/terminal_bench_analysis/tb21_evidence_contract.py`
- Modify: `bench/harbor_adapter/stella_harbor/contract_loader.py`
- Modify: `bench/harbor_adapter/stella_harbor/tb21_evidence.py`
- Modify: `bench/harbor_adapter/tests/test_tb21_evidence.py`
- Modify: `bench/harbor_adapter/tests/test_control_snapshots.py`
- Modify: `bench/harbor_adapter/tests/test_secure_launcher.py`
- Modify: `bench/terminal_bench_analysis/tests/test_tb21_analysis.py`
- Modify: `bench/terminal_bench_analysis/tests/test_github_public_timing.py`
- Create: `bench/harbor_adapter/tests/fixtures/tb21_github_comments_complete.golden.json`

**Interfaces:**
- Produces the exact local result schema with `local_ready`, `launch_authorized`, and seven `deferred_online_checks`.
- With explicit `--github-comments-output`, `--final-ledger-commit`, and the exact paired exports for every required subject, also produces one complete untracked `stella-tb21-github-comments-v3` analyzer input; it never creates a partial map.

- [ ] **Step 1: Write local-result and executable-boundary witnesses**

```python
def test_validate_local_never_authorizes_launch(tmp_repository: Path, capsys: pytest.CaptureFixture[str]) -> None:
    assert main(["validate-local", "--repository", str(tmp_repository)]) == 0
    result = json.loads(capsys.readouterr().out)
    assert result == {
        "local_ready": True,
        "launch_authorized": False,
        "deferred_online_checks": [
            "public_repository_and_main", "owner_unedited_comment",
            "publication_safety_wait_and_final_get",
            "live_runtime_and_provider_identity",
            "live_key_limit_usage_and_credits", "fresh_secure_launch_binding",
            "exact_secret_value_scan",
        ],
    }

def test_validate_local_writes_only_a_complete_comment_map(tmp_repository: Path) -> None:
    output = tmp_repository / "github-comments.json"
    assert run_validate_local(tmp_repository, comment_exports=all_exports(),
                              final_ledger_commit=FINAL_COMMIT,
                              github_comments_output=output) == 0
    assert output.read_bytes() == COMPLETE_COMMENTS_GOLDEN.read_bytes()
```

Add negative cases proving a missing/extra/duplicate export, a nonfinal ledger commit, or any invalid publication leaves the output absent. Run the helper in subprocesses with multiple `PYTHONHASHSEED`, `TZ`, and locale values. Patch AF_INET/AF_INET6, unexpected AF_UNIX, credential environment access, mutating Git verbs, `gh`, `aws`, Harbor run/upload, and provider/GitHub constructors to fail immediately.

- [ ] **Step 2: Run the boundary witnesses and confirm red state**

Run: `cd bench/harbor_adapter && uv run --frozen --extra dev pytest -q tests/test_tb21_evidence.py tests/test_control_snapshots.py -k 'validate_local or tripwire or deterministic'`

Expected: local validation is unimplemented or lacks the exact result.

- [ ] **Step 3: Implement complete offline validation**

Validate canonical files/paths, partition disjointness, lifecycle/global sequence, budget and stage totals, committed Git blobs/ancestry, paired exports, runtime/provider snapshot schemas/digests, prior outcomes, immutable job trees, fresh host/current-host agreement, and credential-pattern absence. When explicitly requested, assemble the complete comments map in memory, validate it through the production public-timing parser, and create the untracked output atomically only after every other check passes. Do not perform any deferred online check.

After formatting the shared contract, update and test the loader's source-frozen digest before the commit.

- [ ] **Step 4: Add production acceptor round-trip tests**

Feed helper-generated partition, ledger, phase manifests, comments, snapshots, host reports, and outcomes into the real analyzer, public-timing verifier, and secure-launcher validators. Assert byte identity, schema identity, and that no completed job artifact-tree digest changes.

Run:

```bash
cd bench/terminal_bench_analysis
uv run --frozen --extra dev pytest -q
cd ../harbor_adapter
uv run --frozen --extra dev pytest -q
```

Expected: both complete suites pass.

- [ ] **Step 5: Commit local validation and integration**

```bash
git add bench/terminal_bench_analysis/tb21_evidence_contract.py \
  bench/terminal_bench_analysis/tests/test_tb21_analysis.py \
  bench/terminal_bench_analysis/tests/test_github_public_timing.py \
  bench/harbor_adapter/stella_harbor/contract_loader.py \
  bench/harbor_adapter/stella_harbor/tb21_evidence.py \
  bench/harbor_adapter/tests/test_tb21_evidence.py \
  bench/harbor_adapter/tests/test_control_snapshots.py \
  bench/harbor_adapter/tests/test_secure_launcher.py \
  bench/harbor_adapter/tests/fixtures/tb21_github_comments_complete.golden.json
git commit -s -m "feat(bench): validate local evidence packages"
```

### Task 8: Document commands and run the complete repository gate

**Files:**
- Modify: `bench/terminal-bench-2.1-protocol.md`
- Modify: `bench/harbor_adapter/README.md`
- Modify: `bench/terminal_bench_analysis/README.md`
- Modify: `bench/README.md`
- Modify: `Makefile`
- Modify: `.github/workflows/ci.yml`

**Interfaces:**
- Documents: all eight helper commands, two snapshot exporters, exact input/output ownership, command ordering, and the post-merge evidence sequence.

- [ ] **Step 1: Add focused helper tests to `bench-gate`**

Ensure `bench-test` collects `test_evidence_filesystem.py`, `test_tb21_evidence.py`, and `test_control_snapshots.py`. CI runs the same frozen lockfiles and target as the local pre-push gate.

- [ ] **Step 2: Document exact command ordering and authority boundaries**

Document: initialize once; preregister/commit/post/export/record; prepare intent; commit/post/export/record; only then use secure launcher; record outcome; freeze screen then confirmatory manifest in the approved order. State repeatedly that local-ready is not launch-authorized and that only the secure launcher can reserve a job or spend.

- [ ] **Step 3: Run fresh complete verification**

```bash
cd bench/terminal_bench_analysis
uv run --frozen --extra dev pytest -q
uv run --frozen --extra dev ruff check .
uv run --frozen --extra dev ruff format --check .
cd ../harbor_adapter
uv run --frozen --extra dev pytest -q
uv run --frozen --extra dev ruff check .
uv run --frozen --extra dev ruff format --check .
cd ../..
git diff --check
make sizes
make gate
```

Expected: all commands exit zero with no paid/network/cloud/GitHub/Harbor mutation.

- [ ] **Step 4: Commit docs and gate wiring**

```bash
git add bench/terminal-bench-2.1-protocol.md bench/harbor_adapter/README.md \
  bench/terminal_bench_analysis/README.md bench/README.md Makefile \
  .github/workflows/ci.yml
git commit -s -m "docs(bench): document local evidence workflow"
```

## Operational handoff after both plans land on public `main`

This sequence is deliberately not executed by either implementation plan:

1. Rebuild both locked Python environments from public `main` and run `make gate`.
2. Run `init-study` against the pinned local comparator and review the exact 10/20/59 partition plus initial ledger.
3. Commit and publish those production evidence bytes separately.
4. Write/review the separate AWS runner-provisioning plan, then execute it through an MFA-backed non-root role and verify its independent hard-cleanup controls before any benchmark credential reaches the host.
5. Append and publish tuning-readiness preregistration.
6. Export fresh runtime/provider snapshots, prepare the readiness intent/host report, and publish its intent attestation.
7. Let the secure launcher repeat every online/current check, create the first paid job only after all public gates pass, record its outcome, and repeat the cycle for each development round.
8. After the eligible development winner is selected, freeze and commit the screen manifest; only then append and publish the sealed-screen preregistration.
9. Prepare and publish the screen intent inside the freshness window, launch through the secure launcher, and record the screen outcome.
10. Only after a passing screen and a new explicit provider/infrastructure authorization, atomically append that authorization while freezing and committing the confirmatory manifest; only then append and publish the confirmatory preregistration and prepare the exact 445-trial intent.

The runner plan is not a paperwork-only gate. Its acceptance tests must prove the approved On-Demand `m7i.2xlarge` native x86_64 host, one encrypted 250-GiB baseline gp3 volume, at least 31 GiB observed RAM and 150 GiB free jobs storage, no instance profile, disabled IMDS, zero running containers at launch, at most one tagged runner/volume/in-use public IPv4 address, no NAT gateway/EIP reservation/snapshot, at most 100 GiB public transfer, a live projected total no greater than $52, and an independent least-privileged stop/terminate/volume-delete schedule inside the absolute $55 cap. That cleanup path carries no benchmark credential and wins over partial-artifact retention.

The current $100 OpenRouter and $55 AWS authorizations cover readiness, development, and one screen only. Confirmatory preparation remains impossible until a passing screen and a new explicit provider/infrastructure authorization exist.
