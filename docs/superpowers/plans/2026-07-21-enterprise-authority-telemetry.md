# Enterprise Authority and Telemetry Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close Stella's Phase 0 authority/verification bypasses and deliver Phase 1 budget, context, privacy, and explicitly enrolled Oxagen Enterprise operational telemetry.

**Architecture:** Compute one monotonic authority policy at settings load, pass it to tool/prompt adapters, and keep all external effects behind host-owned approval ports. Reuse candidate workspaces for witness isolation, preserve terminal outcome truth and cost, and export one content-free post-execution rollup through a managed-only enrolled sink.

**Tech Stack:** Rust 2024, Tokio, Serde, Rusqlite, Reqwest, existing Stella ports, existing candidate worktrees, HMAC-SHA256 enrollment verification using workspace dependencies.

## Global Constraints

- Community Stella performs no enterprise telemetry egress by default.
- Repository configuration can narrow authority but cannot grant it.
- Managed denial is a non-overridable ceiling.
- Model output cannot grant scope, spend, shell, or telemetry authority.
- No production implementation is written before a covering test is observed failing for the intended reason.
- Core remains I/O-free and all cross-crate types round-trip through JSON.
- Operational export contains no prompt, source, path, arguments, results, reasoning, errors, git metadata, memories, or rules.
- Commits use Conventional Commits and include DCO sign-off.

---

### Task 1: Monotonic project and managed authority

**Files:**
- Modify: `stella-cli/src/settings.rs`
- Modify: `stella-cli/src/config.rs`
- Modify: `stella-tools/src/custom.rs`
- Test: co-located test modules in those files

**Interfaces:**
- Produces: `AuthorityPolicy` on `Config`
- Produces: `discover_in_scopes(workspace_root, home, include_workspace)`

- [ ] Add failing tests proving an untrusted project cannot enable `bash` or web, replace an agent prompt, or load a workspace custom tool, and that managed denial survives explicit project trust.
- [ ] Run `cargo test -p stella-cli settings::tests -- --nocapture` and `cargo test -p stella-tools custom::tests -- --nocapture`; confirm the new tests fail because project scope currently wins.
- [ ] Add serializable managed policy structs and compute:

```rust
pub struct AuthorityPolicy {
    pub project_prompts_allowed: bool,
    pub project_custom_tools_allowed: bool,
    pub bash_allowed: bool,
    pub web_allowed: bool,
    pub media_requires_host_approval: bool,
}
```

  Managed `off` is a ceiling; explicit repository trust may grant only within that ceiling. Restore trusted-scope values for untrusted tool and prompt fields.
- [ ] Add `discover_in_scopes`; retain the existing function as a compatibility wrapper and use the policy-aware function in runtime construction.
- [ ] Re-run the two narrow test commands and confirm green.
- [ ] Commit with `git commit -s -m "fix(authority): prevent project capability escalation"`.

### Task 2: Privileged prompt sources and machine-mode approval

**Files:**
- Modify: `stella-cli/src/agent/prompt.rs`
- Modify: `stella-cli/src/rules.rs`
- Modify: `stella-cli/src/extensions.rs`
- Modify: `stella-cli/src/agent.rs`
- Modify: `stella-cli/src/agent/goal.rs`
- Modify: `stella-cli/src/fleet_cmd.rs`
- Test: `stella-cli/src/agent_tests.rs` and co-located modules

**Interfaces:**
- Consumes: `Config::authority`
- Produces: fail-closed headless scope review without altering output serialization

- [ ] Add failing tests proving untrusted workspace rules/prompts/extensions do not enter privileged prompt or guard surfaces and JSON mode cannot auto-approve a large scope.
- [ ] Run the focused tests and confirm failures on the existing unconditional loading and `AutoApproveGate` wiring.
- [ ] Pass `AuthorityPolicy` to prompt/rule/extension loaders; exclude workspace privileged sources when untrusted while retaining user-managed sources.
- [ ] Set headless pipeline configuration to `headless_bypass_scope_review: false` and use a rejecting approval adapter unless an explicit host gate is supplied.
- [ ] Run `cargo test -p stella-cli agent_tests::untrusted_project`, `cargo test -p stella-cli rules::tests::untrusted_project`, and `cargo test -p stella-pipeline headless_scope_review_without_bypass_is_a_named_error`.
- [ ] Commit with `git commit -s -m "fix(cli): separate output format from execution authority"`.

### Task 3: Host-owned paid media approval

**Files:**
- Modify: `stella-media/src/cost_gate.rs`
- Modify: `stella-media/src/lib.rs`
- Modify: `stella-tools/src/media.rs`
- Modify: `stella-tools/src/registry.rs`
- Test: co-located media and registry tests

**Interfaces:**
- Produces: `MediaSpendRequest` and async `MediaSpendGate`
- Default: `DenyMediaSpendGate`

- [ ] Add failing tests proving `confirm_spend: true` cannot authorize video, image generation is denied without a host gate, and an approving fake gate allows exactly one provider submission.
- [ ] Run `cargo test -p stella-tools media::tests` and confirm the host-gate tests fail.
- [ ] Add the host-owned gate, inject it through `RegistryOptions`, remove model-controlled consent from schemas, and gate both image and video before provider submission.
- [ ] Re-run `cargo test -p stella-media` and `cargo test -p stella-tools media::tests`.
- [ ] Commit with `git commit -s -m "fix(media): require host approval for paid generation"`.

### Task 4: Truthful outcomes, settled cost, and stage budgets

**Files:**
- Modify: `stella-core/src/driver.rs`
- Modify: `stella-pipeline/src/pipeline.rs`
- Modify: `stella-pipeline/src/pipeline/tests.rs`
- Modify consumers in `stella-cli`, `stella-fleet`, and `stella-serve`

**Interfaces:**
- Produces: `TurnOutcome::Aborted { reason, cost_usd }`
- Produces: `PipelineStatus::VerificationFailed { verdict }`

- [ ] Add failing core tests proving aborted spend is retained and summary-induced budget breach stops the next provider call.
- [ ] Add failing pipeline tests proving a red final verdict is not `Completed` and an over-cap role call stops the next stage.
- [ ] Run the focused core and pipeline tests and confirm the expected failures.
- [ ] Extend terminal outcome types, propagate them through every exhaustive match, and replace ignored pipeline budget outcomes with a required `Result` returned to `Pipeline::run`.
- [ ] Map verification failure to nonzero CLI/fleet/goal results while retaining cost and evidence.
- [ ] Run `cargo test -p stella-core`, `cargo test -p stella-pipeline`, `cargo test -p stella-fleet`, `cargo test -p stella-serve`, and `cargo test -p stella-cli`.
- [ ] Commit with `git commit -s -m "fix(pipeline): preserve cost and verification truth"`.

### Task 5: Isolated typed witness execution

**Files:**
- Modify: `stella-pipeline/src/witness.rs`
- Modify: `stella-pipeline/src/ports.rs`
- Modify: `stella-pipeline/src/pipeline.rs`
- Modify: `stella-cli/src/candidate_ws.rs`
- Modify: `stella-cli/src/agent/tools.rs`
- Test: pipeline and candidate-workspace tests

**Interfaces:**
- Produces: `TestInvocation { program: String, args: Vec<String> }`
- Produces: candidate-local witness authoring and test execution

- [ ] Add failing tests rejecting shell operators/redirection, detecting tracked production edits, requiring a candidate snapshot for authored witnesses with one candidate, and refusing adoption after failed verification.
- [ ] Run the focused pipeline tests and confirm failures under real-workspace/raw-shell behavior.
- [ ] Parse a strict command vocabulary into program plus argv; route it through a typed test runner rather than `bash -c`.
- [ ] Run witness authoring, baseline, worker, revision, and final verification inside one disposable candidate workspace. Abort if isolation cannot be created; adopt only a passing candidate.
- [ ] Hash the complete accepted witness artifact and reject any non-test or post-baseline mutation.
- [ ] Run `cargo test -p stella-pipeline` and candidate-workspace tests in `stella-cli`.
- [ ] Commit with `git commit -s -m "fix(pipeline): isolate and type witness execution"`.

### Task 6: Context authority and private local state

**Files:**
- Modify: `stella-pipeline/src/ports.rs`
- Modify: `stella-protocol/src/event.rs`
- Modify: `stella-cli/src/memory.rs`
- Modify: `stella-store/src/lib.rs`
- Modify: `stella-store/src/sessions.rs`
- Modify: `stella-store/src/usage.rs`
- Test: protocol, CLI memory, and store tests

**Interfaces:**
- Produces: provenance-preserving `RecalledFrame`
- Produces: owner-only private store creation

- [ ] Add failing tests proving quarantine affects both rendered and pipeline recall, graph provenance remains graph provenance, and private SQLite/session/settings files are owner-only even beneath an existing permissive `.stella` directory.
- [ ] Run focused tests and confirm the current split recall and ambient-umask behavior fail.
- [ ] Make one recall operation own quarantine and projection; add serde-defaulted provenance to the cross-crate event type.
- [ ] Create sensitive directories/files with owner-only permissions from birth and validate existing modes without altering committable project configuration.
- [ ] Run `cargo test -p stella-protocol`, `cargo test -p stella-store`, and focused `stella-cli` memory tests.
- [ ] Commit with `git commit -s -m "fix(context): preserve provenance and private state"`.

### Task 7: Enrolled enterprise operational telemetry

**Files:**
- Create: `stella-store/src/enterprise_telemetry.rs`
- Modify: `stella-store/src/lib.rs`
- Create: `stella-cli/src/enterprise_telemetry.rs`
- Modify: `stella-cli/src/settings.rs`
- Modify: `stella-cli/src/main.rs`
- Modify: execution-finalization callers in `stella-cli`
- Test: co-located store and CLI tests

**Interfaces:**
- Produces: `StellaOperationalEventV1`
- Produces: bounded `EnterpriseTelemetrySpool`
- Produces: managed-only signed enrollment and `telemetry status|flush`

- [ ] Add failing tests for absent/invalid/expired enrollment, forbidden endpoint schemes, signature mismatch, deterministic event IDs, content-free serialization, bounded eviction, retry persistence, and execution success when telemetry fails.
- [ ] Run the focused tests and confirm the types/commands do not exist.
- [ ] Add the transport-neutral operational schema and spool. Derive only from finalized execution rollups; never serialize raw store events.
- [ ] Add managed-only HMAC-SHA256 enrollment verification whose verification secret and bearer credential are environment references. Reject `compliance_audit` event class.
- [ ] Add an async HTTP adapter with bounded request/response bodies and timeouts; flush on explicit command and best-effort lifecycle boundaries.
- [ ] Run `cargo test -p stella-store enterprise_telemetry` and `cargo test -p stella-cli enterprise_telemetry`.
- [ ] Commit with `git commit -s -m "feat(telemetry): add enrolled enterprise operational export"`.

### Task 8: Documentation, broad review, and release gate

**Files:**
- Modify: `AGENTS.md`
- Modify: `CONTRIBUTING.md`
- Modify: `README.md`
- Create: `stella-docs/content/docs/telemetry/index.mdx`
- Modify: `stella-docs/content/docs/examples/enterprise-cloud.mdx`
- Modify: `.agent/memory/shared/lessons.md`
- Create: `.agent/memory/codex/reflections/2026-07-21-enterprise-authority-telemetry.md`

**Interfaces:**
- Documents: community no-egress default and explicit enterprise exception

- [ ] Update every absolute no-phone-home claim to the precise enrolled-enterprise contract and document status/flush, exported fields, excluded fields, delivery semantics, and non-goals.
- [ ] Record the required self-evaluation and deduplicated shared lesson without secrets or customer data.
- [ ] Run `rg -n "no phone-home|only outbound|telemetry" README.md AGENTS.md CONTRIBUTING.md stella-docs` and resolve contradictory claims.
- [ ] Run `cargo fmt --check`, file-size ratchet, Clippy with warnings denied, all workspace tests, release smoke, and supply-chain checks.
- [ ] Dispatch whole-branch correctness and security review; fix all Critical and Important findings and re-run affected tests.
- [ ] Push, wait for GitHub CI, fix until every required check is green, and mark the PR ready for review.
- [ ] Commit with `git commit -s -m "docs(enterprise): define governed telemetry boundary"`.

### Task 9: Enterprise telemetry review hardening

**Files:**
- Modify: `stella-cli/src/enterprise_telemetry.rs`
- Modify: `stella-cli/src/enterprise_telemetry_tests.rs`
- Modify: `stella-cli/src/agent.rs`
- Modify: `stella-cli/src/agent/goal.rs`
- Modify: `stella-cli/src/agent/tools.rs`
- Modify: `stella-cli/src/command_deck.rs`
- Modify: `stella-cli/src/fleet_cmd.rs`
- Modify: `stella-store/src/enterprise_telemetry.rs`
- Modify: `stella-store/tests/enterprise_telemetry.rs`
- Modify: `docs/design/enterprise-authority-telemetry.md`
- Modify: `task-7-report.md`

**Interfaces:**
- Produces: `ExecutionSurface` and a first-line `authorize_execution_surface` gate
- Produces: paged ledger rows carrying a persistent per-export CSPRNG nonce
- Produces: sink-local enqueue eviction, one-time clock rebase, transactional corruption quarantine, and durable corruption telemetry

- [x] Add production-surface tests enumerating raw one-shot, pipeline, goal, fleet, deck, interactive, and candidate/workspace-port construction. Under active `ProcessFree`, allow only the raw registry-only one-shot and return a named authority error before provider/process port construction everywhere else.
- [x] Run the focused CLI tests and observe failures because pipeline modes and `workspace_ports` remain constructible.
- [x] Add a first-line surface gate and a raw tool-surface branch with no MCP, custom tools, interactive tools, skills, discovery actions, hooks, or pipeline ports. Make `workspace_ports` return `Result<WorkspacePorts, String>` and fail closed before custom-tool discovery.
- [x] Add store tests proving capacity never evicts another sink, rollback rebases once without clearing a live concurrent lease, retry deadlines stay within an inclusive 375-second horizon, clone-copied stores receive different first-export nonces while retrying one ledger row keeps its ID, backfill is page bounded across 10,000 rows, completed ledger rows compact under retention, malformed rows quarantine without blocking valid successors, and the largest rounded floating-point cost is rejected.
- [x] Run the focused store tests and observe each new assertion fail against the current implementation.
- [x] Persist `export_nonce` when the ledger row is first inserted and accept it in `OperationalEventContext`; derive the event ID from that nonce. Replace unbounded pending reads with `pending_enterprise_export_page(sink, after, limit)` and compact only completed rows whose idempotency record is already held by the spool/event state.
- [x] Make capacity enforcement select only unleased rows for the inserting sink and drop the new row when global capacity is occupied by other sinks. Persist a per-sink clock anchor; on rollback translate created/retry/lease deadlines once by the observed delta, preserving lease ownership. Use one 375-second maximum retry horizon including jitter.
- [x] Decode and validate payload/event-id/sink consistency inside the claim transaction. Move malformed rows to a quarantine table, increment `corrupt_dropped_rows`, and continue selecting later valid rows within the same bounded scan.
- [x] Replace unchecked float-to-integer rounding with a representable upper-bound check that rejects equality/rounding edges before casting.
- [x] Re-run focused RED tests to green, then full store/CLI/tools tests, production-surface tests, formatting, Clippy with warnings denied, file-size gate, and diff checks.
- [x] Update the design and `task-7-report.md` with exact invariants and fresh command evidence.
- [x] Commit with `git commit -s -m "fix(telemetry): close process and spool review gaps"`; do not push.

### Task 10: Enterprise telemetry generation and storage bounds

**Files:**
- Modify: `stella-store/src/enterprise_telemetry.rs`
- Create: `stella-store/src/enterprise_telemetry/migrations.rs`
- Modify: `stella-store/tests/enterprise_telemetry.rs`
- Modify: `stella-cli/src/enterprise_telemetry.rs`
- Modify: `stella-cli/src/enterprise_telemetry_tests.rs`
- Modify: `docs/design/enterprise-authority-telemetry.md`
- Modify: `task-7-report.md`

**Interfaces:**
- Produces: generation-fenced retry claims and a fresh retry wall-clock read
- Produces: versioned, cursor-based legacy nonce migration with a 1,024-row startup budget
- Produces: a 128-row fixed-fingerprint quarantine sample with explicit status accounting

- [x] Add RED regressions for a claim racing a concurrent clock rollback, a 50,257-row legacy ledger, and repeated corruption including an oversized identifier.
- [x] Persist and return a per-sink clock generation; transactionally fence/rebase retry against the current generation and read fresh wall time before every delivery retry.
- [x] Replace the all-row legacy migration with resumable four-by-256 cursor batches and durable version/progress state.
- [x] Hash corrupt identifiers, retain only the newest 128 diagnostic rows, report diagnostic rows/bytes and physical bytes, and bound WAL/journal growth.
- [x] Run focused and full store/CLI/tools suites, Clippy, formatting, file-size and diff gates.
- [x] Update design/report with exact invariants and fresh counts.
- [x] Create a new DCO-signed commit without pushing.
