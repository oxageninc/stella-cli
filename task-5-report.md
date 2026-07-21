# Task 5 report: isolated, typed witness execution

## Outcome

Task 5 is complete. Model- and user-authored test text now crosses a strict
parser into `TestInvocation { program, args }` and runs through a direct
process `TestRunner`, never through `bash -c`. An authored witness is created,
baselined, exercised, revised, and finally verified inside the same disposable
candidate workspace, including when only one candidate is requested.

Only a candidate with a passing final verdict may be adopted. Failed,
aborted, or red candidates are removed, and witness-isolation failure aborts
before witness authoring can touch the session tree.

## TDD evidence

RED was established before implementation:

- The parser tests failed to compile because `TestInvocation` and
  `parse_test_invocation` did not exist. They then proved shell operators,
  redirection, expansion, unknown programs, and malformed quoting are rejected.
- Witness-artifact tests failed to compile before
  `validate_witness_artifact` existed. They now prove tracked edits,
  pre-existing untracked edits, non-test files, and multi-file authoring are
  rejected.
- A same-length edit with restored mtime passed the legacy `len:mtime`
  fingerprint. It became detectable only after fingerprints changed to
  SHA-256 over the complete file bytes.
- A direct-runner test initially had no typed execution boundary. It now
  proves a redirection token remains a literal argv item and creates no file.
- The one-candidate witness regression initially touched the session
  `RepoStatusPort` and panicked. It now authors and verifies entirely inside
  one disposable candidate.
- The isolation-failure regression initially reached session state. It now
  aborts before emitting the witness stage.
- The final-red regression initially logged winner adoption. It now logs no
  adoption and removes every candidate.
- The tracked-production-edit regression initially allowed witness author
  contamination. It now aborts the candidate, removes it, and never adopts.
- Task 4's post-witness routing-cost regression caught an ordering change; the
  worker is still resolved after paid witness authoring so settled cost is
  retained on routing failure.

Review follow-up also used RED-first regressions:

- Witness, worker, and revision hooks initially recorded the session workspace
  as their process directory. They now all record the disposable candidate
  root, while deliberately distinct session tools and status ports remain
  untouched.
- A candidate mutated after verification was initially adoptable. Final
  verification now seals an immutable private commit; both the pre-adoption
  drift check and adoption itself reject any live-tree divergence, and adoption
  applies the exact baseline-to-seal bytes.
- A post-baseline witness mutation initially reached a passing model judge. It
  now hard-fails before judge evaluation and can never be overridden.
- Symlink/hardlink witness regressions initially crossed the artifact boundary.
  Witness identity now requires a regular, single-link file and fingerprints
  bytes, type, mode, link count, and symlink target.
- The real candidate status wrapper initially returned no artifact identity
  even though its lower-level adapter supported one. A real Git-worktree RED
  regression now proves candidate-local witness identities are forwarded and
  match the candidate delta fingerprint.
- `src/witness_backdoor.rs`, language-mismatched witnesses, absolute/parent
  paths, runner retarget flags (including after `--`), quoted operators,
  Unicode whitespace/lookalikes, environment prefixes, and path executables
  were initially accepted by one or more permissive paths. The typed parser,
  test-shape check, and invocation-to-language check now reject them.
- The runner-locality test initially inherited Git repository pointer
  variables. Child test processes now run with candidate `PWD`/cwd and all
  Git repository pointer variables explicitly removed.
- Raw diagnostic command strings initially exposed a shell-capable boundary.
  A closed `DiagnosticInvocation` enum now maps only to direct fixed Git argv;
  a metacharacter-bearing untracked path remains a literal argument.

A second review follow-up closed the remaining filesystem edge cases:

- A sealed witness is committed into the candidate and therefore disappears
  from `git ls-files --others`. The regression first failed with a false
  witness-deletion abort. Final verification now compares the current direct
  `artifact_identity(path)` with the full identity captured at authoring,
  independent of Git tracked/untracked classification.
- A real Git candidate regression exercises author → seal → final identity
  verification → exact adoption and explicitly proves the seal reclassifies
  the intact witness without invalidating it. Scripted direct-identity
  regressions still hard-fail a byte or metadata mutation before judge review.
- Artifact identity no longer performs separate path metadata and path reads.
  Unix opens once with `O_NOFOLLOW | O_CLOEXEC`, gets metadata and bytes from
  that handle, requires `nlink == 1`, and compares path device/inode with the
  opened handle both before and after reading. A deterministic rename-and-
  replacement regression proves path retargeting is rejected.
- Windows opens with `FILE_FLAG_OPEN_REPARSE_POINT`, but stable Rust does not
  expose a by-handle link count. Witness identity therefore fails closed on
  Windows instead of manufacturing a single-link result. The platform behavior
  regression records that contract; Unix additionally proves hardlinks and
  symlinks are rejected.

## Implementation notes

- `TestInvocation` and `TestRunner` form a typed, shell-free test boundary.
  The CLI adapter launches the known program with its explicit argv and a
  candidate workspace root. Fixed Git diff probes use a separate closed
  `DiagnosticInvocation` vocabulary and direct process execution.
- Configured test commands are parsed before the first paid pipeline stage.
  Witness commands are parsed immediately after authoring or repair.
- The accepted command vocabulary is intentionally narrow: Cargo test and
  nextest, common JavaScript package test runners, pytest, Go test, and .NET
  test.
- Witness validation compares complete tracked and untracked before/after maps,
  accepts exactly one newly created language-compatible test artifact, and
  validates its filesystem identity through `symlink_metadata`.
- Both session and candidate repo-status adapters hash complete bytes plus file
  identity metadata. Tracked deletions receive a sentinel so they remain
  visible.
- Every authored witness gets a candidate-local authoring pass. Its baseline,
  worker execution, revision loop, hooks, tamper checks, and final verification
  reuse that candidate's cwd, tools, status ports, and typed test runner.
- Adoption is gated on `verdict.passed`, an unchanged verified seal, and the
  immutable sealed commit. Every other workspace is removed; the existing
  recovery exception for a passing winner whose adoption itself conflicts
  remains intact.
- Task-specific production and test modules keep the existing pipeline files
  below their size ratchets.

## Verification

- `cargo test -p stella-pipeline`: 130 unit tests and 4 replay tests passed.
- `cargo test -p stella-cli`: 336 tests passed, including all 12 candidate
  workspace tests and the typed-runner/fingerprint regressions.
- `cargo clippy --workspace --all-targets -- -D warnings`: passed.
- `cargo fmt --all -- --check`: passed.
- `scripts/check-file-sizes.sh`: passed for all 293 tracked Rust files.
- `git diff --check`: passed.

## Self-review

- Searched all pipeline command execution sites and confirmed test observations
  use `TestRunner`; fixed diagnostics use only the closed direct-Git runner.
- Reviewed every candidate exit path. Isolation failures, witness failures,
  worker-routing failures, red verification, and non-winning candidates all
  clean up without adoption.
- Corrected stale witness documentation that still described a permissive
  multi-file watchlist; the accepted artifact is now exactly one new test file.
- Re-ran the full pipeline and CLI suites after the module extraction and
  Clippy fixes.

## Concerns

The invocation boundary prevents command retargeting and shell interpretation,
but accepted test suites are repository code and are not an OS-level sandbox.
They can still perform arbitrary actions available to the Stella process. A
separate sandbox/container boundary would be required to constrain hostile test
code itself. No push was performed.
