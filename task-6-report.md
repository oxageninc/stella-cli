# Task 6 report: context authority and private local state

## Outcome

Task 6 is complete. Rendered prompt recall and pipeline recall now share one
authoritative operation. It fans out through the OCP host, projects every frame
without erasing its provider, source, kind, URI, or derivation method, and reads
the quarantine set afresh for every call. A citation marked untruthful enough
to cross the quarantine threshold therefore disappears from both consumers on
the next recall, even when the session was already open.

Sensitive workspace state now lives below `.stella/private/`, an owner-only
`0700` boundary, with regular files created or repaired to `0600` and terminal
symlinks rejected. SQLite main/WAL/SHM files therefore share one private
directory instead of racing in the mixed project `.stella` directory. The
mixed directory and committable settings and rules retain their
repository-selected modes. User settings, context/store/usage/codegraph/fleet
databases, reflections, session records, journals, snapshots, notifications,
MCP OAuth credentials, MCP config, and TUI debug logs use hardened persistence.
Other platforms fail closed until equivalent owner-only/no-follow primitives
exist.

## TDD evidence

RED was established before implementation:

- Protocol tests referencing provider, kind, URI, and method failed to compile
  because `ContextFrameRef` did not carry those fields. The additive fields are
  now serde-defaulted, and both provenance-rich and pre-provenance JSON streams
  parse and round-trip.
- OCP host tests initially returned bare `ContextFrame` values, losing which
  provider leg produced each frame. `AttributedContextFrame` now keeps provider
  identity through sorting, deduplication, and budget capping.
- A graph-frame regression initially projected every result as hard-coded
  `memory`. It now remains a `code-graph`/`symbol` frame with its file URI and
  `tree-sitter/symbol-extract` method intact.
- A session-open quarantine regression initially kept returning a memory after
  two untruthful citations were recorded. Both rendered and structured recall
  now call the same fresh-quarantine operation and exclude it immediately.
- With a permissive existing `.stella`, SQLite created `context.db` and its
  sidecars with ambient modes. The whole database family now lives under a
  mode-at-create `0700` `.stella/private/` boundary without chmodding the mixed
  project directory, `settings.json`, or `.stella/rules/*.md`.
- Store, usage, session, journal, notification, and user-settings mode tests
  initially observed permissive files/directories. They now prove owner-only
  creation and repair, including an isolated subprocess with `umask(0)`.
- Symlink regressions initially allowed ordinary filesystem APIs to follow
  attacker-controlled targets. Hardened opens now reject terminal symlinks and
  preserve the outside target byte-for-byte.
- The hardened journal refactor exposed an existing torn-tail recovery
  dependency: an append-only descriptor could not inspect its final byte and a
  focused regression returned two records instead of three. Opening the same
  no-follow descriptor for read plus append restored recovery without a second
  path traversal.
- Review regressions proved provider-local deduplication, local-memory-only
  quarantine, A/B suppression before skill loading, and multi-hop provenance
  source/method selection.
- A legacy database with live WAL/SHM sidecars initially admitted a partial
  multi-rename migration. It now fails closed, remains byte-identical, and
  asks the operator to close/checkpoint before the main DB is atomically moved.
- Re-review first compiled the OAuth migration witness against a path-only
  helper and failed because callers could not receive resolver errors. The
  helper now returns `Result`, every login/logout/status/agent/TUI caller
  propagates or visibly reports failure, and a safe legacy token store migrates
  before the OAuth manager opens it.
- The OAuth staging witness initially failed because a workspace that had not
  opened the main store had no generated ignore file. Creating any private
  workspace state now best-effort creates `.stella/.gitignore` with `private/`,
  and a real `git check-ignore` test proves token files are ignored.
- Direct graph queries and graph availability initially treated safe legacy
  indexes as absent. Both now preflight the shared SQLite resolver and migrate
  safe legacy state; direct queries surface unsafe legacy-parent errors without
  claiming there is no index. Storage and observatory commands apply the same
  fallible preflight before their lower-level readers.
- Session-memory construction initially discarded secure-path failures with
  `.ok()?`. The resolver now emits one explicit "memory disabled" warning when
  warnings are enabled, remains quiet only when explicitly requested, and
  never converts an unsafe legacy layout into ordinary absence.
- The final ignore-file regression seeded the exact previous generated bytes
  plus custom rules and initially remained unchanged. Resolution now appends a
  final `private/` exactly once through a no-follow atomic replacement, keeps
  custom byte order and the committable file mode, fsyncs file and directory,
  and proves the migrated OAuth path with `git check-ignore`.
- A permissive legacy code graph initially let a schema write land because the
  live storage gate received an empty snapshot; startup population likewise
  could not return an error. Both now fail through the shared resolver before
  the lower snapshot loader. The unsafe write stays absent, while an owner-safe
  legacy graph migrates and the same write succeeds.

## Implementation notes

- `SessionMemory::recalled_frames` is the single recall operation used by both
  `recall_block` and `ContextRecallPort::recall`. It owns A/B suppression, OCP
  fan-out, lossless projection, and the current quarantine query.
- `RecalledFrame` and protocol `ContextFrameRef` carry provider, source, kind,
  URI, and method. Event construction and TUI consumers were updated to preserve
  the expanded shape rather than inventing fallback provenance.
- Memory citation affordances remain limited to frames whose actual kind is
  `memory`; graph and other grounding frames are rendered but never enter the
  memory citation/promotion loop.
- `stella-store/src/private.rs` centralizes secure directory, regular-file,
  atomic-write, and SQLite-open primitives. SQLite receives a canonical parent
  path and `SQLITE_OPEN_NOFOLLOW`; regular files use `O_NOFOLLOW | O_CLOEXEC`,
  creation mode `0600`, descriptor-based mode repair, and a single-link check.
- Workspace SQLite consumers resolve store, context, codegraph, and fleet files
  through the same `.stella/private/` boundary. Safe closed legacy databases
  migrate once; permissive legacy parents, ambiguous old/new files, and live
  sidecar families remain untouched with actionable errors.
- Sensitive atomic replacements fsync both the file and containing directory.
  Reflections, MCP OAuth/config, settings, and TUI debug writers reject
  terminal symlinks and repair owner-only modes where supported.
- Fresh private directories use mode-at-create `0700`. Existing directories
  known to contain only private state are repaired to `0700`. Existing mixed
  project `.stella` directories are validated but deliberately not chmodded.
- Private workspace-state resolution creates or idempotently upgrades
  `.stella/.gitignore`, preserving custom contents and mode while appending
  `private/`. Documentation treats `.stella/private/` as authoritative and
  records one-time migration from safe closed legacy files.
- `stella_tools::graph::load_storage_snapshot` is the fallible governance
  boundary. Startup schema-index population and every gated storage write use
  it; resolver errors propagate visibly instead of becoming `false`, an empty
  snapshot, or a successful ungated write.
- User settings use secure atomic replacement because they may contain inline
  API keys. Project settings keep the ordinary write path so their committed
  file mode is not silently changed.
- Focused private-state and persistence modules keep the repository file-size
  ratchet green.

## Verification

- `cargo test -p stella-protocol`: 43 tests passed.
- `cargo test -p stella-pipeline`: 130 unit tests and 4 replay tests passed.
- `cargo test -p stella-context`: 53 tests passed.
- `cargo test -p stella-store`: 87 tests passed.
- `cargo test -p stella-graph`: 65 unit + 18 integration tests passed; 1
  environment-dependent watcher test ignored.
- `cargo test -p stella-tools`: 333 unit + 18 integration tests passed; 1
  sandbox test ignored. The localhost tracker and web integration suites passed
  outside the filesystem/network sandbox.
- `cargo test -p stella-cli`: 352 tests passed after the projection split and
  private-state preflight additions.
- `cargo test -p stella-tui`: 487 unit + 5 render tests passed; 1 TTY test
  ignored.
- `cargo test -p stella-mcp`: 68 unit + 22 integration + 1 doc test passed.
- `cargo test -p stella-observatory`: 22 tests passed.
- `cargo clippy --workspace --all-targets -- -D warnings`: passed.
- `cargo fmt --all -- --check`: passed.
- `make sizes`: all 299 tracked Rust files passed.
- `pnpm --dir stella-docs typecheck` and `pnpm --dir stella-docs build`:
  passed; 81 static pages generated.
- `git diff --check`: passed.

## Self-review

- Traced both recall consumers to the same operation and searched the projection
  path for hard-coded memory attribution.
- Reviewed creation, reopening, replacement, and read paths for every hardened
  private-state artifact; reads and writes reject symlinks rather than relying
  on a check followed by a normal path open.
- Confirmed project `.stella`, settings, and canonical rules are never passed to
  the private-directory repair primitive.
- Re-ran the full affected protocol, pipeline, context, store, graph, tools,
  CLI, TUI, MCP, and Observatory suites after the module split.
- Searched authoritative docs and CLI help for direct workspace state paths;
  the only remaining legacy path mentions are migration documentation and
  migration/error regression fixtures.
- Audited every lower code-graph snapshot load. Governance callers now pass
  through the fallible resolver boundary; remaining direct opens are index
  construction, already-preflighted query/advisory paths, UI-only views, or
  tests rather than write-authorization decisions.

## Concerns

Secure owner-only file creation currently relies on Unix primitives. Non-Unix
private-file writes fail closed instead of silently using weaker ambient
permissions or following reparse points. A future Windows implementation should
use explicit ACLs at creation plus handle-based reparse/link validation before
enabling private persistence there. No push was performed.
